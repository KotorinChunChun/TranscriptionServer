use crate::{
    app::App,
    catalog, platform,
    store::{active, now},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;
pub struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    _scope: Arc<platform::Job>,
    pub model: String,
    pub device: String,
    pub requests: u64,
    key: String,
    pub used: Instant,
    pub pinned: bool,
}
impl Session {
    pub async fn unload(&mut self) -> Result<()> {
        self._scope.terminate()?;
        tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .context("Whisperの終了未確認")??;
        crate::scheduler::wait_scope(&self._scope).await
    }
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    async fn new(app: &App, config: &Value, id: Option<&str>) -> Result<Self> {
        let tools = app.tools();
        let (exe, args) = tools
            .whisper
            .as_ref()
            .context("Whisperワーカーがありません")?;
        let tools = app.tools();
        let mut command = tools.command(exe);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1")
            .env("HF_HUB_DISABLE_PROGRESS_BARS", "1");
        for key in [
            "MEETING_SERVER_API_KEY",
            "TRANSCRIPTION_GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "TRANSCRIPTION_API_KEY",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn()?;
        app.supervisor.assign(&child)?;
        let scope = if let Some(id) = id {
            app.job_scope(id)?
        } else {
            Arc::new(platform::Job::new()?)
        };
        scope.assign(&child)?;
        Ok(Self {
            input: child.stdin.take().context("worker stdin")?,
            output: BufReader::new(child.stdout.take().context("worker stdout")?),
            child,
            _scope: scope,
            model: config["model"].as_str().unwrap().into(),
            device: config["device"].as_str().unwrap().into(),
            requests: 0,
            key: session_key(config),
            used: Instant::now(),
            pinned: false,
        })
    }
}
/// Per-worker threads; explicit administrator override is retained.
pub(crate) fn whisper_threads(app: &App) -> u16 {
    app.runtime.config.threads.unwrap_or_else(|| {
        let workers = app.scheduler.lock().unwrap().settings.max_jobs;
        std::thread::available_parallelism()
            .map(|v| (v.get() / workers).clamp(1, 4) as u16)
            .unwrap_or(1)
    })
}
/// 外部プログラムへ渡す引数の列を作る。
fn owned<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(String::from).collect()
}
fn remember_languages(app: &App, id: &str, values: &Value) -> Result<()> {
    let Some(values) = values.as_array().filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let mut languages = app.store.get(id)?["metadata"]["detectedLanguages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for value in values
        .iter()
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 32))
    {
        if !languages.contains(value) {
            languages.push(value.clone());
        }
    }
    app.store
        .update(id, json!({"metadata":{"detectedLanguages":languages}}))?;
    Ok(())
}
fn session_key(config: &Value) -> String {
    format!(
        "{}:{}:{}",
        config["model"], config["device"], config["threads"]
    )
}
pub async fn preload(
    app: &App,
    lane: usize,
    model: &str,
    device: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let config =
        json!({"action":"load","model":model,"device":device,"threads":whisper_threads(app)});
    let mut session = Session::new(app, &config, None).await?;
    session
        .input
        .write_all(format!("{config}\n").as_bytes())
        .await?;
    session.input.flush().await?;
    let mut ready = false;
    loop {
        let mut line = String::new();
        let n = tokio::select! {
            _ = cancel.cancelled() => bail!("ロードを中止しました"),
            v = session.output.read_line(&mut line) => v?,
        };
        if n == 0 || line.len() > 2 * 1024 * 1024 {
            bail!("Whisperワーカーが終了しました")
        };
        let message: Value = serde_json::from_str(&line)?;
        match message["type"].as_str() {
            Some("ready") => ready = true,
            Some("done") if ready => break,
            Some("error") => bail!(
                "{}",
                message["message"]
                    .as_str()
                    .unwrap_or("ロードに失敗しました")
            ),
            _ => bail!("Whisperロードの応答が不正です"),
        }
    }
    session.pinned = true;
    *app.slots[lane].lock().await = Some(session);
    Ok(())
}
pub async fn execute(app: Arc<App>, lane: usize, id: &str) -> bool {
    let Ok(job) = app.store.get(id) else {
        return false;
    };
    let token = app
        .running
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .unwrap_or_default();
    let linked = token.child_token();
    let outcome = tokio::select! {
        _ = app.stop.cancelled() => { linked.cancel(); Err(anyhow::anyhow!("停止しました")) },
        result = run_with_fallback(&app,lane,id,&linked) => result,
    };
    if let Err(error) = outcome {
        if !token.is_cancelled() && !app.stop.is_cancelled() {
            let message = error.to_string().chars().take(2000).collect::<String>();
            *app.worker_error.lock().unwrap() = Some(message.clone());
            let diarization = if job["diarizationEnabled"] == true {
                "error"
            } else {
                "not_performed"
            };
            let _ = app.store.update(id, json!({"status":"error","phase":"error","error":message,
                "message":"文字起こしを完了できませんでした。","metadata":{"diarizationStatus":diarization}}));
        }
        return false;
    }
    !token.is_cancelled() && !app.stop.is_cancelled()
}
async fn run_with_fallback(
    app: &App,
    lane: usize,
    id: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    match run_job(app, lane, id, cancel).await {
        Err(gpu_error) if !cancel.is_cancelled() && !app.stop.is_cancelled() => {
            let job = app.store.get(id)?;
            if job["device"] == "auto"
                && job["metadata"]["executionDevice"] == "gpu"
                && job["metadata"]["gpuAttemptStarted"] == true
                && app.store.retry_gpu_on_cpu(id, &gpu_error.to_string())?
            {
                crate::scheduler::cleanup(app, lane, false).await?;
                crate::scheduler::cleanup_scopes(app, id).await?;
                run_job(app, lane, id, cancel).await.map_err(|cpu_error| {
                    anyhow::anyhow!("GPU試行: {gpu_error}; CPU試行: {cpu_error}")
                })
            } else {
                Err(gpu_error)
            }
        }
        result => result,
    }
}
async fn run_job(app: &App, lane: usize, id: &str, cancel: &CancellationToken) -> Result<()> {
    let job = app.store.get(id)?;
    if cancel.is_cancelled() || !active(&job) {
        return Ok(());
    }
    let queued = job["metadata"]["queuedAt"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| {
            (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                .num_milliseconds()
                .max(0) as f64
                / 1000.
        })
        .unwrap_or(0.);
    let gpu = job["metadata"]["executionDevice"] != "cpu"
        && !matches!(job["engine"].as_str(), Some("openai" | "compatible"));
    if job["metadata"]["dequeuedAt"].is_null() {
        app.store.update(
            id,
            json!({"status":"running","phase":"preparing",
                "message":"音声を確認し、モデルを取得・読み込みしています。",
                "metadata":{"dequeuedAt":now(),"queueSeconds":queued,"deviceWaitEnabled":gpu}}),
        )?;
    }
    if cancel.is_cancelled() {
        return Ok(());
    }
    app.store.update(id, json!({"status":"running"}))?;
    if job["metadata"]["startedAt"].is_null() {
        app.store.update(
            id,
            json!({"phase":"preparing","metadata":{"startedAt":now()}}),
        )?;
    }
    let source = app.store.dir.join(id).join("source.media");
    let tools = app.tools();
    let probe = tools.ffprobe.as_ref().context("FFprobeがありません")?;
    let raw = run_capture(
        app,
        probe,
        &[
            "-v".into(),
            "error".into(),
            "-show_entries".into(),
            "format=duration".into(),
            "-of".into(),
            "json".into(),
            source.to_string_lossy().into(),
        ],
        cancel,
        id,
        Duration::from_secs(60),
    )
    .await?;
    let info: Value = serde_json::from_slice(&raw)?;
    let duration: f64 = info["format"]["duration"]
        .as_str()
        .context("音声の長さがありません")?
        .parse()?;
    let start = job["rangeStart"].as_f64().unwrap_or(0.);
    let end = job["rangeEnd"].as_f64().unwrap_or(duration).min(duration);
    if !duration.is_finite() || duration <= 0. || start >= end || !end.is_finite() {
        bail!("音声の長さ/処理範囲が不正です")
    };
    if job["workload"] == "realtime" && end - start > 30. {
        bail!("リアルタイム枠は30秒以下です。長い録音はfile枠を使用してください。")
    }
    app.store.update(
        id,
        json!({"duration":duration,"rangeEnd":end,"phase":"preparing",
            "metadata":{"audioDuration":duration,"rangeStart":start,"rangeEnd":end}}),
    )?;
    let mut metadata = json!({"completedAt":now()});
    if gpu && job["device"] == "auto" && !app.store.begin_gpu_attempt(id)? {
        return Ok(());
    }
    match job["engine"].as_str().unwrap_or("haas") {
        crate::integrated::ENGINE => {
            metadata =
                crate::integrated::run(app, lane, id, &job, &source, start, end, cancel).await?;
        }
        crate::nemo::ENGINE => {
            *app.native_slots[lane].lock().await = None;
            metadata = nemo_asr(app, id, &job, &source, start, end, cancel).await?;
        }
        "diarization" => {
            *app.native_slots[lane].lock().await = None;
            let spans = diarize(app, id, &source, start, end, cancel, &job, false).await?;
            for span in &spans {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                app.store.append(id, json!({"start":span["start"],"end":span["end"],"text":"",
                    "speaker":span["speaker"],"speakers":[span["speaker"]],"speakerSource":"automatic",
                    "speakerNeedsReview":false,"timestampType":"diarization_segment"}))?;
            }
            let status = if spans.is_empty() {
                "no_speakers"
            } else {
                "complete"
            };
            metadata = json!({"runtime":"speech.cpp","task":"diarization","asrPerformed":false,
                "diarizationModel":"nemotron-3-diarization","diarizationStatus":status,
                "diarizationSpanCount":spans.len(),"actualDevice":job["metadata"]["executionDevice"],
                "timestampType":"diarization_segment"});
        }
        "whisper" => {
            metadata = whisper(app, lane, id, &job, &source, start, end, cancel).await?;
        }
        "openai" | "compatible" => {
            metadata = crate::telemetry::measure(
                &app.store,
                id,
                "provider_processing",
                "外部API処理全体（音声準備・通信・応答待ち）",
                cancel,
                None,
                provider(app, id, &job, &source, start, end, cancel),
            )
            .await?;
        }
        _ => {
            let mut resident = app.native_slots[lane].lock().await;
            let expected = format!(
                "{}/{}",
                job["engine"].as_str().unwrap(),
                job["modelId"].as_str().unwrap()
            );
            let reused = resident.as_ref().is_some_and(|s| {
                s.id == expected
                    && s.device == job["metadata"]["executionDevice"].as_str().unwrap_or("cpu")
            });
            if reused {
                let outcome = native_resident(
                    app,
                    id,
                    &job,
                    &source,
                    start,
                    end,
                    cancel,
                    resident.as_mut().unwrap(),
                )
                .await;
                outcome?;
                app.store.update(
                    id,
                    json!({"metadata":{"modelReused":true,"runtime":"speech.cpp worker protocol4"}}),
                )?;
            } else {
                if let Some(s) = resident.as_mut() {
                    s.unload().await?;
                }
                *resident = None;
                drop(resident);
                native(app, id, &job, &source, start, end, cancel).await?;
            }
            let diarization = match (
                job["diarizationEnabled"] == true,
                app.store.has_speakers(id)?,
            ) {
                (false, _) => "not_performed",
                (true, true) => "complete",
                (true, false) => "no_speakers",
            };
            metadata = json!({"runtime":"speech.cpp",
                "actualDevice":job["metadata"]["executionDevice"],"diarizationStatus":diarization});
        }
    }
    if !cancel.is_cancelled() {
        metadata["completedAt"] = now().into();
        let message = if job["engine"] == "diarization" {
            "話者分離が完了しました。結果を保存しました。"
        } else {
            "文字起こしが完了しました。結果を保存しました。"
        };
        app.store.update(
            id,
            json!({"status":"complete","phase":"complete","processedUntil":end,
                "message":message,"metadata":metadata}),
        )?;
    }
    Ok(())
}
fn stderr_log(app: &App, id: &str, stderr: tokio::process::ChildStderr) {
    let path = app.store.dir.join(id).join("runtime.log");
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = stderr;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .ok();
        let mut bytes = 0;
        let mut buffer = [0; 4096];
        while let Ok(n) = reader.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            if bytes < 65536
                && let Some(file) = &mut file
            {
                let _ = file.write_all(&buffer[..n.min(65536 - bytes)]).await;
            }
            bytes += n;
        }
    });
}
async fn run_capture(
    app: &App,
    exe: &Path,
    args: &[String],
    cancel: &CancellationToken,
    id: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let scope = app.job_scope(id)?;
    let tools = app.tools();
    let mut command = tools.command(exe);
    if app.tools().nemo.as_deref() == Some(exe) {
        crate::nemo::configure(&mut command, exe);
    }
    let mut child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("SPEECH_NO_UPDATE_CHECK", "1")
        .spawn()?;
    app.supervisor.assign(&child)?;
    scope.assign(&child)?;
    stderr_log(app, id, child.stderr.take().unwrap());
    let mut out = child.stdout.take().unwrap().take(8_000_001);
    let mut raw = vec![];
    let (kind, label) = if app.tools().ffprobe.as_deref() == Some(exe) {
        ("media_probe", "音声・動画の点検")
    } else if app.tools().ffmpeg.as_deref() == Some(exe) {
        ("audio_preparation", "音声の変換・切り出し")
    } else if args.iter().any(|a| a == "nemotron-3-diarization") {
        ("diarization", "話者分離・モデル準備")
    } else if args.first().is_some_and(|a| a == "vad") {
        ("vad", "音声区間の検出・モデル準備")
    } else {
        ("recognition", "文字起こし・モデル準備")
    };
    let finished = async {
        out.read_to_end(&mut raw).await?;
        if !child.wait().await?.success() {
            bail!("実行プログラムが失敗しました。runtime.logを確認してください。")
        }
        if raw.len() > 8_000_000 {
            bail!("実行結果が大きすぎます")
        }
        Ok::<_, anyhow::Error>(raw)
    };
    crate::telemetry::measure(&app.store, id, kind, label, cancel, Some(&scope), async {
        tokio::select! {
            _ = cancel.cancelled() => bail!("処理を中止しました"),
            result = tokio::time::timeout(timeout, finished) => {
                result.context("処理がタイムアウトしました")?
            }
        }
    })
    .await
}
pub(crate) async fn wav(
    app: &App,
    id: &str,
    source: &Path,
    start: f64,
    length: f64,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let tools = app.tools();
    let ffmpeg = tools.ffmpeg.as_ref().context("FFmpegがありません")?;
    run_capture(
        app,
        ffmpeg,
        &[
            "-hide_banner".into(),
            "-nostdin".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
            "-ss".into(),
            start.to_string(),
            "-i".into(),
            source.to_string_lossy().into(),
            "-t".into(),
            length.to_string(),
            "-map".into(),
            "0:a:0".into(),
            "-vn".into(),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            "16000".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            path.to_string_lossy().into(),
        ],
        cancel,
        id,
        Duration::from_secs_f64((length * 2.).max(120.)),
    )
    .await?;
    Ok(())
}
/// 話者分離を実行する処理方法。単独話者分離はジョブの選択、Nemotron ASR併用は常にCPU。
fn diarization_device(job: &Value) -> Result<&str> {
    Ok(if job["engine"] == "diarization" {
        job["metadata"]["executionDevice"]
            .as_str()
            .filter(|d| matches!(*d, "cpu" | "gpu"))
            .context("話者分離の処理方法が不正です")?
    } else if job["engine"] == crate::nemo::ENGINE {
        "cpu"
    } else if job["device"] == "gpu" {
        "gpu"
    } else {
        "cpu"
    })
}
/// 認識と話者分離を同時に進めるか。認識がGPU候補・話者分離がCPUで計算資源が分かれる場合だけ並行する。
/// 両方CPUでは実測（i5-12500T）で奪い合いにより0.85〜0.97倍へ遅くなり、両方GPUはVRAM競合のため直列にする。
fn diarization_parallel(job: &Value) -> bool {
    job["diarizationEnabled"] == true
        && job["metadata"]["executionDevice"]
            .as_str()
            .is_some_and(|d| d != "cpu")
        && diarization_device(job).is_ok_and(|d| d == "cpu")
}
#[allow(clippy::too_many_arguments)]
async fn diarize(
    app: &App,
    id: &str,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
    job: &Value,
    concurrent: bool,
) -> Result<Vec<Value>> {
    let standalone = job["engine"] == "diarization";
    let nemo = job["engine"] == crate::nemo::ENGINE;
    let model = if standalone || nemo {
        catalog::require_diarization_file(&app.tools(), &app.supervisor)
            .await?
            .to_string_lossy()
            .into_owned()
    } else {
        "nemotron-3-diarization".into()
    };
    let device = diarization_device(job)?;
    // 並行時は認識側がphaseを持つため、話者分離はmetadataだけ更新する。
    let started =
        json!({"diarizationModel":"nemotron-3-diarization","diarizationStatus":"running"});
    app.store.update(
        id,
        if concurrent {
            json!({"message":"話者分離と文字起こしを並行して処理しています。","metadata":started})
        } else {
            json!({"phase":"diarizing","message":"Nemotronで話者を分離しています。","metadata":started})
        },
    )?;
    let path = app.store.dir.join(id).join("diarization.wav");
    let result = async {
        wav(app, id, source, start, end - start, &path, cancel).await?;
        let tools = app.tools();
        let speech = tools.speech.as_ref().context("speech.cppがありません")?;
        let mut args = vec!["diarize".into(), model];
        args.extend(owned(["--format", "json", "--device", device]));
        args.extend(owned(["--streaming-mode", "offline"]));
        args.extend(["--threads".into(), whisper_threads(app).to_string()]);
        args.push(path.to_string_lossy().into());
        let raw = run_capture(app, speech, &args, cancel, id, Duration::from_secs(86400)).await?;
        let payload: Value = serde_json::from_slice(&raw)?;
        let spans = diarization_spans(&payload, start, end)?;
        let status = if spans.is_empty() {
            "no_speakers"
        } else {
            "complete"
        };
        let finished = json!({"diarizationStatus":status,"diarizationSpanCount":spans.len(),
            "diarizationDevice":device});
        app.store.update(
            id,
            if concurrent {
                json!({"metadata":finished})
            } else {
                json!({"phase":"diarizing","metadata":finished})
            },
        )?;
        Ok::<_, anyhow::Error>(spans)
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}
/// 認識結果の保存単位。字幕と進捗の順序を保ったまま保留・再生する。
enum Pending {
    Segment(Value),
    Update(Value),
}
/// 話者分離を認識と並行するとき、話者区間が出るまで字幕と進捗の保存を保留する。
/// 区間確定後に従来と同じassign_speakersで話者を付けて同じ順序で保存するため、
/// 保存結果は「話者分離→認識」の直列実行と同一になり、未割当字幕が途中で見えることもない。
struct SpeakerGate<'a> {
    enabled: bool,
    spans: &'a OnceLock<Vec<Value>>,
    held: Vec<Pending>,
}
impl<'a> SpeakerGate<'a> {
    fn new(enabled: bool, spans: &'a OnceLock<Vec<Value>>) -> Self {
        Self {
            enabled,
            spans,
            held: vec![],
        }
    }
    fn waiting(&self) -> bool {
        self.enabled && self.spans.get().is_none()
    }
    fn spans(&self) -> Option<&'a [Value]> {
        self.spans.get().map(Vec::as_slice)
    }
    fn segment(&mut self, store: &crate::store::Store, id: &str, segment: Value) -> Result<()> {
        self.segments(store, id, vec![segment])
    }
    fn segments(
        &mut self,
        store: &crate::store::Store,
        id: &str,
        segments: Vec<Value>,
    ) -> Result<()> {
        self.held.extend(segments.into_iter().map(Pending::Segment));
        self.flush(store, id)
    }
    fn update(&mut self, store: &crate::store::Store, id: &str, changes: Value) -> Result<()> {
        self.held.push(Pending::Update(changes));
        self.flush(store, id)
    }
    /// 話者区間が確定していれば保留分を順に保存する。未確定なら何もしない。
    fn flush(&mut self, store: &crate::store::Store, id: &str) -> Result<()> {
        if self.waiting() {
            return Ok(());
        }
        let spans = if self.enabled { self.spans() } else { None };
        let mut batch = vec![];
        for item in std::mem::take(&mut self.held) {
            match item {
                Pending::Segment(mut segment) => {
                    if let Some(spans) = spans {
                        assign_speakers(&mut segment, spans)
                    }
                    batch.push(segment);
                }
                Pending::Update(changes) => {
                    store.append_many(id, std::mem::take(&mut batch))?;
                    store.update(id, changes)?;
                }
            }
        }
        store.append_many(id, batch)
    }
    /// 認識の終了後に呼ぶ。話者分離がまだなら表示を話者分離中へ切り替える。
    fn recognition_done(&mut self, store: &crate::store::Store, id: &str) -> Result<()> {
        if self.waiting() {
            store.update(
                id,
                json!({"phase":"diarizing","message":"Nemotronで話者を分離しています。"}),
            )?;
        }
        self.flush(store, id)
    }
    fn status(&self) -> &'static str {
        match self.spans() {
            _ if !self.enabled => "not_performed",
            Some([]) => "no_speakers",
            _ => "complete",
        }
    }
}
/// 話者分離（有効時）と認識を実行する。並行時は片方の失敗・中止で他方も止める。
async fn with_speakers(
    parallel: bool,
    diarization: impl std::future::Future<Output = Result<()>>,
    recognition: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    if parallel {
        tokio::try_join!(diarization, recognition)?;
    } else {
        diarization.await?;
        recognition.await?;
    }
    Ok(())
}
/// FFmpegの時刻引数と同じく、10進表記を小数6桁（マイクロ秒）で切り捨てる。
fn ffmpeg_micros(value: f64) -> Option<u64> {
    let text = value.to_string();
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let micros = format!("{:0<6}", &fraction[..fraction.len().min(6)]);
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(micros.parse().ok()?)
}
/// マイクロ秒を16kHzのサンプル数へ四捨五入する。
fn samples_16k(micros: u64) -> Option<usize> {
    usize::try_from(micros.checked_mul(32_000)?.checked_add(1_000_000)? / 2_000_000).ok()
}
/// 16kHz mono s16le WAVの先頭部分から、PCM本体のファイル内位置とバイト数を返す。
fn wav_data_16k(header: &[u8], file_len: u64) -> Result<(u64, u64)> {
    if header.len() < 12 || &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        bail!("VAD用WAVが不正です")
    }
    let (mut pos, mut format) = (12usize, false);
    while pos + 8 <= header.len() {
        let size = u32::from_le_bytes(header[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match &header[pos..pos + 4] {
            b"fmt " => {
                let f = header
                    .get(body..body + 16)
                    .context("VAD用WAVの形式が不正です")?;
                let tag = u16::from_le_bytes([f[0], f[1]]);
                let channels = u16::from_le_bytes([f[2], f[3]]);
                let rate = u32::from_le_bytes(f[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes([f[14], f[15]]);
                if !matches!(tag, 1 | 0xFFFE) || channels != 1 || rate != 16000 || bits != 16 {
                    bail!("VAD用WAVが16kHz mono 16bit PCMではありません")
                }
                format = true;
            }
            b"data" if format => {
                let offset = body as u64;
                let len = (size as u64).min(file_len.saturating_sub(offset));
                return Ok((offset, len - len % 2));
            }
            b"data" => bail!("VAD用WAVの形式情報がありません"),
            _ => (),
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    bail!("VAD用WAVに音声がありません")
}
/// `ffmpeg -ss left -i WAV -t length -f s16le`と同じサンプル範囲（先頭, 個数）を返す。
/// FFmpegは時刻をマイクロ秒へ切り捨ててからサンプルへ四捨五入する（実測でバイト一致を確認）。
/// 1サンプル未満の長さはFFmpegが最小フレームを返すため、Noneを返して従来処理に任せる。
fn ffmpeg_sample_range(total: u64, left: f64, length: f64) -> Option<(u64, u64)> {
    let first = samples_16k(ffmpeg_micros(left)?)? as u64;
    let count = samples_16k(ffmpeg_micros(length)?)? as u64;
    if count == 0 {
        return None;
    }
    let first = first.min(total);
    Some((first, first.saturating_add(count).min(total) - first))
}
/// VAD用WAVから発話区間のPCMを読む。FFmpegを発話ごとに起動しない。
pub(crate) struct PcmReader {
    file: tokio::fs::File,
    offset: u64,
    samples: u64,
}
impl PcmReader {
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(path).await?;
        let file_len = file.metadata().await?.len();
        let mut header = vec![0; file_len.min(65536) as usize];
        file.read_exact(&mut header).await?;
        let (offset, len) = wav_data_16k(&header, file_len)?;
        Ok(Self {
            file,
            offset,
            samples: len / 2,
        })
    }
    pub(crate) async fn read(&mut self, left: f64, length: f64) -> Result<Option<Vec<u8>>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let Some((first, count)) = ffmpeg_sample_range(self.samples, left, length) else {
            return Ok(None);
        };
        self.file
            .seek(std::io::SeekFrom::Start(self.offset + first * 2))
            .await?;
        let mut pcm = vec![0; (count * 2) as usize];
        self.file.read_exact(&mut pcm).await?;
        Ok(Some(pcm))
    }
}
/// Nemotron ASRへ渡す1塊の上限秒数。分割が長いほど処理時間が急増するため短くまとめる。
const NEMO_CHUNK_MAX: f64 = 30.;
/// 無音のない1発話を切らずに渡す上限秒数（文脈を保つためのユーザー指定）。
const NEMO_SPEECH_MAX: f64 = 300.;
/// これより短い塊は話者が替わっても切らない（相づちで細切れにしない）。
const NEMO_SPEAKER_MIN: f64 = 4.;
/// 発話区間（処理範囲内の秒、話者）を時刻順にNemotron ASRの塊へまとめる。
/// 塊の先頭から次の発話の終わりまでが上限内で、話者が替わらない（または塊が短い）間は同じ塊へ足す。
/// 塊同士の間の無音は送らず、余白で重なった部分は二重に送らない。
fn nemo_chunks(regions: &[(f64, f64, Option<&str>)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = vec![];
    let (mut floor, mut last) = (0f64, None);
    for &(left, right, speaker) in regions {
        let left = left.max(floor);
        if right <= left {
            continue;
        }
        let changed = matches!((last, speaker), (Some(a), Some(b)) if a != b);
        match out.last_mut() {
            Some(chunk)
                if right - chunk.0 <= NEMO_CHUNK_MAX
                    && !(changed && chunk.1 - chunk.0 >= NEMO_SPEAKER_MIN) =>
            {
                chunk.1 = right
            }
            _ => {
                let mut from = left;
                while right - from > NEMO_SPEECH_MAX {
                    out.push((from, from + NEMO_SPEECH_MAX));
                    from += NEMO_SPEECH_MAX;
                }
                out.push((from, right));
            }
        }
        floor = right;
        if speaker.is_some() {
            last = speaker;
        }
    }
    out
}
/// 話者区間（絶対秒）のうち、区間との重なりが最長の話者。
fn dominant_speaker(spans: &[Value], left: f64, right: f64) -> Option<&str> {
    let mut best = (None, 0f64);
    for span in spans {
        let (Some(a), Some(b)) = (span["start"].as_f64(), span["end"].as_f64()) else {
            continue;
        };
        let overlap = right.min(b) - left.max(a);
        if overlap > best.1 {
            best = (span["speaker"].as_str(), overlap);
        }
    }
    best.0
}
/// 16kHz mono s16leのPCMにWAVヘッダーを付ける。
fn wav_16k(pcm: &[u8]) -> Vec<u8> {
    let size = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + size).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&32000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}
fn diarization_spans(payload: &Value, start: f64, end: f64) -> Result<Vec<Value>> {
    let mut spans = vec![];
    for row in payload["segments"]
        .as_array()
        .context("話者分離結果が不正です")?
    {
        let left = row["start"].as_f64().context("話者時刻が不正です")?;
        let right = row["end"].as_f64().context("話者時刻が不正です")?;
        let speaker = row["speaker"]
            .as_u64()
            .filter(|v| *v < 8)
            .context("話者番号が不正です")?;
        if !left.is_finite() || !right.is_finite() || left < 0. || right <= left {
            bail!("話者区間が不正です")
        }
        let (l, r) = ((left + start).min(end), (right + start).min(end));
        if r > l {
            spans.push(json!({"start":l,"end":r,"speaker":format!("話者{}",speaker+1)}));
        }
    }
    spans.sort_by(|a, b| {
        a["start"]
            .as_f64()
            .unwrap()
            .total_cmp(&b["start"].as_f64().unwrap())
            .then_with(|| {
                a["end"]
                    .as_f64()
                    .unwrap()
                    .total_cmp(&b["end"].as_f64().unwrap())
            })
    });
    Ok(spans)
}
#[allow(clippy::too_many_arguments)]
async fn whisper(
    app: &App,
    lane: usize,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
) -> Result<Value> {
    let enabled = job["diarizationEnabled"] == true;
    let parallel = diarization_parallel(job);
    let cell = OnceLock::new();
    let mut gate = SpeakerGate::new(enabled, &cell);
    let mut metadata = Value::Null;
    let diarization = async {
        if enabled {
            let _ = cell.set(diarize(app, id, source, start, end, cancel, job, parallel).await?);
        }
        Ok(())
    };
    let recognition = async {
        metadata =
            whisper_recognize(app, lane, id, job, source, start, end, cancel, &mut gate).await?;
        Ok(())
    };
    with_speakers(parallel, diarization, recognition).await?;
    gate.flush(&app.store, id)?;
    metadata["diarizationStatus"] = gate.status().into();
    Ok(metadata)
}
/// Whisperワーカーの字幕が処理範囲内の正しい区間で、本文が大きすぎないことを確かめる。
fn check_whisper_segment(segment: &Value, start: f64, end: f64) -> Result<()> {
    let left = segment["start"].as_f64().context("字幕時刻が不正です")?;
    let right = segment["end"].as_f64().context("字幕時刻が不正です")?;
    if !left.is_finite()
        || !right.is_finite()
        || left < start
        || right > end + 0.001
        || right <= left
        || segment["text"].as_str().is_none_or(|s| s.len() > 800000)
    {
        bail!("Whisper字幕が不正です")
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn whisper_recognize(
    app: &App,
    lane: usize,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
    gate: &mut SpeakerGate<'_>,
) -> Result<Value> {
    let config = json!({"reportStages":true,"source":source,"ffmpeg":app.tools().ffmpeg,
        "start":start,"end":end,"model":job["modelId"],
        "device":job["metadata"]["executionDevice"],"threads":whisper_threads(app),
        "recognitionOptions":crate::recognition::for_job(job)});
    let mut slot = app.slots[lane].lock().await;
    let keep_pinned = slot.as_ref().is_some_and(|s| {
        s.pinned
            && s.model == job["modelId"].as_str().unwrap_or("")
            && s.device == job["metadata"]["executionDevice"].as_str().unwrap_or("cpu")
    });
    if slot.as_mut().is_some_and(|s| {
        s.key != session_key(&config) || s.child.try_wait().ok().flatten().is_some()
    }) {
        slot.as_mut().unwrap().unload().await?;
        *slot = None
    }
    if slot.is_none() {
        let mut session =
            Session::new(app, &config, if keep_pinned { None } else { Some(id) }).await?;
        session.pinned = keep_pinned;
        *slot = Some(session)
    }
    let scope = slot.as_ref().unwrap()._scope.clone();
    let mut detail: Option<crate::telemetry::Stage<'_>> = None;
    let mut detail_kind = String::new();
    let work = async {
        let session = slot.as_mut().unwrap();
        session.requests += 1;
        session
            .input
            .write_all(format!("{}\n", config).as_bytes())
            .await?;
        session.input.flush().await?;
        let warnings = if end - start > 300. {
            vec!["Whisperは5分単位です。区切り付近を確認してください。"]
        } else {
            vec![]
        };
        let mut metadata = json!({"runtime":"faster-whisper worker",
            "timestampType":"whisper_segment","audioChunkSeconds":300,"warnings":warnings});
        if !gate.enabled {
            metadata["diarizationStatus"] = "not_performed".into();
        }
        loop {
            let mut line = String::new();
            let n = crate::telemetry::read_line(&mut session.output, &mut line, cancel, || {
                if let Some(stage) = &mut detail {
                    stage.sample(&scope)
                }
            })
            .await?;
            if n == 0 || line.len() > 2 * 1024 * 1024 {
                bail!("Whisperワーカーの出力が不正または中断しました")
            }
            let message: Value = serde_json::from_str(&line)?;
            match message["type"].as_str() {
                Some("stage") => {
                    let kind = message["stage"]
                        .as_str()
                        .context("Whisper工程がありません")?;
                    let label = match kind {
                        "model_load" => "モデルの取得・ロード",
                        "audio_preparation" => "音声の変換・切り出し",
                        "recognition" => "文字起こし",
                        _ => bail!("Whisper工程が不正です"),
                    };
                    match message["event"].as_str() {
                        Some("start") if detail.is_none() => {
                            detail_kind = kind.into();
                            detail = Some(crate::telemetry::Stage::new(
                                &app.store, id, kind, label, cancel,
                            )?);
                        }
                        Some(event @ ("complete" | "error"))
                            if detail_kind == kind && detail.is_some() =>
                        {
                            detail.take().unwrap().finish(event)?;
                        }
                        _ => bail!("Whisper工程の順序が不正です"),
                    }
                }
                Some("ready") => {
                    for key in ["device", "computeType", "modelReused", "modelLoadSeconds"] {
                        let name = if key == "device" { "actualDevice" } else { key };
                        metadata[name] = message[key].clone()
                    }
                    let version = message["version"].as_str().unwrap_or("");
                    metadata["runtime"] = format!("faster-whisper {version}").into();
                    app.store
                        .update(id, json!({"phase":"transcribing","metadata":metadata}))?;
                }
                Some("segment") => {
                    let segment = message["segment"].clone();
                    check_whisper_segment(&segment, start, end)?;
                    gate.segment(&app.store, id, segment)?;
                }
                Some("language") => {
                    remember_languages(app, id, &json!([message["language"]]))?;
                    metadata["detectedLanguages"] =
                        app.store.get(id)?["metadata"]["detectedLanguages"].clone();
                }
                Some("progress") => {
                    let done = message["processedUntil"]
                        .as_f64()
                        .context("進捗が不正です")?;
                    if done.is_finite() && start <= done && done <= end {
                        gate.update(&app.store, id, json!({"processedUntil":done}))?;
                    }
                }
                Some("done") => break,
                Some("error") => bail!(
                    "{}",
                    message["message"]
                        .as_str()
                        .unwrap_or("Whisperに失敗しました")
                ),
                _ => bail!("Whisperの出力種別が不正です"),
            }
        }
        session.used = Instant::now();
        gate.recognition_done(&app.store, id)?;
        Ok::<_, anyhow::Error>(metadata)
    };
    let label = "Whisper処理全体（モデル準備・変換・認識）";
    let result = crate::telemetry::measure(
        &app.store,
        id,
        "worker_processing",
        label,
        cancel,
        Some(&scope),
        work,
    )
    .await;
    if let Some(mut stage) = detail {
        stage.finish(if cancel.is_cancelled() {
            "cancelled"
        } else if result.is_ok() {
            "complete"
        } else {
            "error"
        })?;
    }
    result
}
pub fn union_duration(mut ranges: Vec<(f64, f64)>) -> f64 {
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut total, mut end) = (0., f64::NEG_INFINITY);
    for (left, right) in ranges {
        if right > end {
            total += right - left.max(end);
            end = right
        }
    }
    total
}
pub fn assign_speakers(segment: &mut Value, spans: &[Value]) {
    let start = segment["start"].as_f64().unwrap();
    let end = segment["end"].as_f64().unwrap();
    let mut intervals = vec![];
    let mut groups: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for span in spans {
        let left = span["start"].as_f64().unwrap_or(0.).max(start);
        let right = span["end"].as_f64().unwrap_or(0.).min(end);
        if right > left {
            let label = span["speaker"].as_str().unwrap_or("").to_string();
            intervals.push(json!({"start":left,"end":right,"speaker":label}));
            groups.entry(label).or_default().push((left, right));
        }
    }
    let mut candidates = groups
        .into_iter()
        .map(|(speaker, ranges)| {
            let first = ranges.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
            (speaker, union_duration(ranges), first)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.total_cmp(&b.2)));
    let total: f64 = candidates.iter().map(|r| r.1).sum();
    let coverage = union_duration(
        intervals
            .iter()
            .map(|r| (r["start"].as_f64().unwrap(), r["end"].as_f64().unwrap()))
            .collect(),
    ) / (end - start).max(0.001);
    let dominant = candidates.first().is_some_and(|r| r.1 / total >= 0.75);
    let label = if dominant {
        candidates[0].0.clone()
    } else {
        let mut ordered = candidates.clone();
        ordered.sort_by(|a, b| a.2.total_cmp(&b.2));
        ordered
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    segment["speaker"] = label.into();
    segment["speakerSource"] = if candidates.is_empty() {
        "unassigned"
    } else {
        "automatic"
    }
    .into();
    segment["speakerNeedsReview"] = (!dominant || coverage < 0.5).into();
    segment["speakerCoverage"] = coverage.into();
    segment["speakerIntervals"] = json!(intervals);
    segment["speakerCandidates"] = json!(
        candidates
            .iter()
            .map(|r| json!({"speaker":r.0,"duration":r.1,"share":r.1/total}))
            .collect::<Vec<_>>()
    );
}
fn speaker_label(value: &Value) -> String {
    if let Some(number) = value.as_u64().filter(|v| *v < 8) {
        return format!("話者{}", number + 1);
    }
    let label = value.as_str().unwrap_or("");
    if label.len() == 1 && matches!(label.as_bytes()[0], b'A'..=b'H') {
        format!("話者{}", label.as_bytes()[0] - b'A' + 1)
    } else {
        label.chars().take(100).collect()
    }
}
#[derive(Default)]
pub struct NativeParser {
    items: HashMap<String, (Option<f64>, Option<f64>)>,
    completed: HashSet<String>,
}
impl NativeParser {
    pub fn feed(&mut self, event: &Value, offset: f64, diar: bool) -> Result<Option<Value>> {
        let kind = event["type"].as_str().unwrap_or("");
        if matches!(
            kind,
            "error" | "conversation.item.input_audio_transcription.failed"
        ) {
            bail!("speech.cppの認識に失敗しました")
        };
        let id = event["item_id"].as_str().unwrap_or("").to_string();
        match kind {
            "input_audio_buffer.speech_started" => {
                self.items.entry(id).or_default().0 = Some(
                    event["audio_start_ms"]
                        .as_f64()
                        .context("発言開始時刻が不正です")?
                        / 1000.,
                )
            }
            "input_audio_buffer.speech_stopped" => {
                self.items.entry(id).or_default().1 = Some(
                    event["audio_end_ms"]
                        .as_f64()
                        .context("発言終了時刻が不正です")?
                        / 1000.,
                )
            }
            "conversation.item.input_audio_transcription.completed" => {
                if !self.completed.insert(id.clone()) {
                    return Ok(None);
                }
                let (left, right) = self
                    .items
                    .remove(&id)
                    .context("発言時刻がありません。推測せず停止します")?;
                let (start, end) = (
                    left.context("開始時刻がありません")?,
                    right.context("終了時刻がありません")?,
                );
                if !start.is_finite() || !end.is_finite() || start < 0. || end < start {
                    bail!("発言区間が不正です")
                };
                let text = event["transcript"].as_str().unwrap_or("").trim();
                if text.is_empty() || end == start {
                    return Ok(None);
                }
                let mut segment = json!({"start":offset+start,"end":offset+end,"text":text,
                    "speaker":"","speakerSource":"unassigned","speakerNeedsReview":diar,
                    "timestampType":"vad_utterance","nativeItemId":id,
                    "transcriptionNeedsReview":event["stop"]=="model_limit"});
                let spans = event["speakers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| {
                        let left = s["start_ms"].as_f64()? / 1000.;
                        let right = s["end_ms"].as_f64()? / 1000.;
                        let label = speaker_label(&s["speaker"]);
                        (left.is_finite() && right.is_finite() && right > left && !label.is_empty())
                            .then(
                                || json!({"start":offset+left,"end":offset+right,"speaker":label}),
                            )
                    })
                    .collect::<Vec<_>>();
                assign_speakers(&mut segment, &spans);
                if !diar {
                    segment["speakerNeedsReview"] = false.into()
                }
                return Ok(Some(segment));
            }
            _ => (),
        };
        Ok(None)
    }
}
#[allow(clippy::too_many_arguments)]
async fn native_resident(
    app: &App,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
    session: &mut crate::models::NativeSession,
) -> Result<()> {
    let path = app.store.dir.join(id).join("native-vad.wav");
    let enabled = job["diarizationEnabled"] == true;
    let parallel = diarization_parallel(job);
    let cell = OnceLock::new();
    let mut gate = SpeakerGate::new(enabled, &cell);
    let result = async {
        let diarization = async {
            if enabled {
                let _ =
                    cell.set(diarize(app, id, source, start, end, cancel, job, parallel).await?);
            }
            Ok(())
        };
        let recognition = async {
            wav(app, id, source, start, end - start, &path, cancel).await?;
            let tools = app.tools();
            let speech = tools.speech.as_ref().context("speech.cppがありません")?;
            let mut args = owned(["vad", "silero-vad", "--format", "json"]);
            args.extend(owned([
                "--speech-pad-ms",
                "300",
                "--min-silence-duration-ms",
                "500",
            ]));
            args.extend(owned(["--max-speech-duration-s", "10"]));
            args.push(path.to_string_lossy().into());
            let raw =
                run_capture(app, speech, &args, cancel, id, Duration::from_secs(3600)).await?;
            let vad: Value = serde_json::from_slice(&raw)?;
            let mut reader = PcmReader::open(&path).await?;
            for region in vad["regions"].as_array().context("VADの時刻がありません")? {
                let left = region["start"].as_f64().context("VAD開始時刻が不正です")?;
                let right = region["end"].as_f64().context("VAD終了時刻が不正です")?;
                if !left.is_finite()
                    || !right.is_finite()
                    || left < 0.
                    || right <= left
                    || right > end - start + 0.01
                    || right - left > 15.
                {
                    bail!("VAD区間が不正です")
                }
                if cancel.is_cancelled() {
                    bail!("処理を中止しました")
                }
                let pcm = match reader.read(left, right - left).await? {
                    Some(pcm) => pcm,
                    // 1サンプル未満の区間だけはFFmpegの出力に合わせる。
                    None => {
                        let tools = app.tools();
                        let ffmpeg = tools.ffmpeg.as_ref().context("FFmpegがありません")?;
                        let mut args = owned(["-v", "error", "-nostdin", "-ss"]);
                        args.push(left.to_string());
                        args.extend(["-i".into(), path.to_string_lossy().into()]);
                        args.extend(["-t".into(), (right - left).to_string()]);
                        args.extend(owned(["-ac", "1", "-ar", "16000", "-f", "s16le", "pipe:1"]));
                        run_capture(app, ffmpeg, &args, cancel, id, Duration::from_secs(120))
                            .await?
                    }
                };
                let scope = session.scope();
                let answer = crate::telemetry::measure(
                    &app.store,
                    id,
                    "recognition",
                    "常駐モデルで文字起こし",
                    cancel,
                    Some(&scope),
                    async {
                        tokio::time::timeout(
                            Duration::from_secs(600),
                            session.transcribe(&pcm, crate::recognition::for_job(job), cancel),
                        )
                        .await
                        .context("常駐モデルの認識がタイムアウトしました")?
                    },
                )
                .await?;
                let until = (start + right).min(end);
                remember_languages(app, id, &answer["languages"])?;
                if let Some(text) = answer["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    let segment = json!({"start":start+left,"end":until,"text":text,
                        "speaker":"","speakerSource":"unassigned","speakerNeedsReview":false,
                        "timestampType":"vad_utterance",
                        "transcriptionNeedsReview":answer["stop"]=="model_limit"});
                    gate.segment(&app.store, id, segment)?;
                }
                gate.update(
                    &app.store,
                    id,
                    json!({"phase":"transcribing","processedUntil":until}),
                )?;
            }
            gate.recognition_done(&app.store, id)
        };
        with_speakers(parallel, diarization, recognition).await?;
        gate.flush(&app.store, id)
    }
    .await;
    let _ = tokio::fs::remove_file(path).await;
    result
}
async fn native(
    app: &App,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
) -> Result<()> {
    let scope = app.job_scope(id)?;
    let tools = app.tools();
    let ffmpeg = tools.ffmpeg.as_ref().context("FFmpegがありません")?;
    let tools = app.tools();
    let speech = tools.speech.as_ref().context("speech.cppがありません")?;
    let tools = app.tools();
    let mut decode = tools
        .command(ffmpeg)
        .args([
            "-hide_banner",
            "-nostdin",
            "-v",
            "error",
            "-ss",
            &start.to_string(),
            "-i",
            &source.to_string_lossy(),
            "-t",
            &(end - start).to_string(),
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    app.supervisor.assign(&decode)?;
    scope.assign(&decode)?;
    stderr_log(app, id, decode.stderr.take().unwrap());
    let tools = app.tools();
    let mut command = tools.command(speech);
    command.args([
        "asr",
        job["modelId"].as_str().unwrap(),
        "--vad",
        "silero-vad",
        "--format",
        "json",
        "--rate",
        "16000",
        "--device",
        job["metadata"]["executionDevice"].as_str().unwrap(),
    ]);
    command.args(crate::recognition::cli_args(job));
    if job["diarizationEnabled"] == true {
        command.args([
            "--diarize",
            "nemotron-3-diarization",
            "--streaming-mode",
            "offline",
        ]);
    }
    command
        .arg("--threads")
        .arg(whisper_threads(app).to_string());
    command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("SPEECH_NO_UPDATE_CHECK", "1");
    let mut asr = command.spawn()?;
    app.supervisor.assign(&asr)?;
    scope.assign(&asr)?;
    stderr_log(app, id, asr.stderr.take().unwrap());
    let mut input = asr.stdin.take().unwrap();
    let mut audio = decode.stdout.take().unwrap();
    let pump = tokio::spawn(async move {
        let result = tokio::io::copy(&mut audio, &mut input).await;
        let _ = input.shutdown().await;
        result
    });
    let mut reader = BufReader::new(asr.stdout.take().unwrap());
    let mut parser = NativeParser::default();
    let mut stage = crate::telemetry::Stage::new(
        &app.store,
        id,
        "recognition",
        "文字起こし・音声変換・VAD・モデル準備（内部内訳なし）",
        cancel,
    )?;
    let diarization = if job["diarizationEnabled"] == true {
        "complete"
    } else {
        "not_performed"
    };
    app.store.update(
        id,
        json!({"phase":"transcribing",
            "metadata":{"runtime":"speech.cpp","diarizationStatus":diarization}}),
    )?;
    let result = async {
        loop {
            let mut line = String::new();
            let n = crate::telemetry::read_line(&mut reader, &mut line, cancel, || {
                stage.sample(&scope)
            })
            .await?;
            if n == 0 {
                break;
            }
            if line.len() > 2 * 1024 * 1024 {
                bail!("ネイティブ出力が大きすぎます")
            };
            if line.trim().is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(&line)?;
            remember_languages(app, id, &event["languages"])?;
            if let Some(segment) = parser.feed(&event, start, job["diarizationEnabled"] == true)? {
                if segment["end"].as_f64().unwrap() > end + 0.1 {
                    bail!("発言時刻が処理範囲を超えています")
                };
                app.store.append(id, segment)?;
            }
        }
        let finished = async {
            let speech = asr.wait().await?;
            let audio = decode.wait().await?;
            pump.await??;
            if !speech.success() || !audio.success() {
                bail!("ネイティブ認識に失敗しました。runtime.logを確認してください。")
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            _ = cancel.cancelled() => bail!("処理を中止しました"),
            result = finished => result,
        }
    }
    .await;
    stage.finish(if cancel.is_cancelled() {
        "cancelled"
    } else if result.is_ok() {
        "complete"
    } else {
        "error"
    })?;
    result
}
#[allow(clippy::too_many_arguments)]
async fn nemo_asr(
    app: &App,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
) -> Result<Value> {
    let enabled = job["diarizationEnabled"] == true;
    let parallel = diarization_parallel(job);
    let cell = OnceLock::new();
    let mut gate = SpeakerGate::new(enabled, &cell);
    let tools = app.tools();
    let model = crate::nemo::require_model(&tools)?;
    let tools = app.tools();
    let exe = tools.nemo.as_ref().context("NeMo-Speech.cppがありません")?;
    let device = if job["metadata"]["executionDevice"] == "gpu" {
        "vulkan:0"
    } else {
        "cpu"
    };
    let language = crate::nemo::language(job["metadata"]["language"].as_str())?;
    let realtime = job["workload"] == "realtime";
    // 無音・話者の切れ目で区切るにはVADが必要。未導入なら取得せず従来の300秒固定分割にする。
    let vad = if realtime {
        None
    } else {
        catalog::installed_vad_file(&app.tools(), &app.supervisor).await
    };
    let path = app.store.dir.join(id).join("nemo.wav");
    let whole = app.store.dir.join(id).join("nemo-range.wav");
    let mut chunking = if realtime {
        "realtime"
    } else if vad.is_some() {
        "vad"
    } else {
        "fixed"
    };
    let mut chunk_count = 0usize;
    let result = async {
        let mut estimated = false;
        let mut languages = HashSet::new();
        let diarization = async {
            if enabled {
                let _ =
                    cell.set(diarize(app, id, source, start, end, cancel, job, parallel).await?);
            }
            Ok(())
        };
        let recognition = async {
            let mut transcribe = async |gate: &mut SpeakerGate<'_>,
                                        wav: &Path,
                                        left: f64,
                                        right: f64|
                   -> Result<()> {
                let message = if gate.waiting() {
                    "話者分離とNemotron ASRの文字起こしを並行して処理しています。"
                } else {
                    "Nemotron ASRで文字起こししています。"
                };
                app.store
                    .update(id, json!({"phase":"transcribing","message":message}))?;
                let mut args = vec!["transcribe".into(), wav.to_string_lossy().into()];
                args.extend(["--model".into(), model.to_string_lossy().into()]);
                args.extend(owned(["--language", language, "--device", device]));
                args.extend(owned(["--format", "json", "--no-batching", "--no-warmup"]));
                if realtime {
                    args.push("--stream".into());
                }
                let timeout = Duration::from_secs_f64(((right - left) * 16.).max(120.));
                let raw = run_capture(app, exe, &args, cancel, id, timeout).await?;
                let payload: Value = serde_json::from_slice(&raw)?;
                for value in payload["languages"].as_array().into_iter().flatten() {
                    if let Some(value) = value.as_str().filter(|v| v.len() < 32) {
                        languages.insert(value.to_owned());
                    }
                }
                let segments = crate::nemo::segments(&payload, left, right)?;
                estimated |= segments
                    .iter()
                    .any(|segment| segment["transcriptionNeedsReview"] == true);
                gate.segments(&app.store, id, segments)?;
                gate.update(&app.store, id, json!({"processedUntil":right}))
            };
            if let Some(vad) = &vad {
                app.store.update(
                    id,
                    json!({"phase":"transcribing","message":"無音の区間を調べています。"}),
                )?;
                wav(app, id, source, start, end - start, &whole, cancel).await?;
                // 並行時は話者区間の到着が認識と前後し得るため、結果を安定させるよう使わない。
                let spans = if enabled && !parallel {
                    gate.spans()
                } else {
                    None
                };
                if spans.is_some() {
                    chunking = "vad_speaker";
                }
                let chunks =
                    nemo_vad_chunks(app, id, vad, &whole, start, end, spans, cancel).await?;
                chunk_count = chunks.len();
                let mut reader = PcmReader::open(&whole).await?;
                for (left, right) in chunks {
                    if cancel.is_cancelled() {
                        bail!("処理を中止しました")
                    }
                    let Some(pcm) = reader.read(left, right - left).await? else {
                        continue;
                    };
                    tokio::fs::write(&path, wav_16k(&pcm)).await?;
                    transcribe(&mut gate, &path, start + left, start + right).await?;
                }
                gate.update(&app.store, id, json!({"processedUntil":end}))?;
            } else {
                let mut offset = start;
                while offset < end {
                    let length = 300f64.min(end - offset);
                    wav(app, id, source, offset, length, &path, cancel).await?;
                    transcribe(&mut gate, &path, offset, offset + length).await?;
                    offset += length;
                    chunk_count += 1;
                }
            }
            gate.recognition_done(&app.store, id)
        };
        with_speakers(parallel, diarization, recognition).await?;
        gate.flush(&app.store, id)?;
        let mut languages: Vec<_> = languages.into_iter().collect();
        languages.sort();
        let max = if vad.is_some() { NEMO_CHUNK_MAX } else { 300. };
        let timestamp = if estimated {
            "nemo_chunk_estimated"
        } else {
            "nemo_alignment"
        };
        Ok::<_, anyhow::Error>(json!({"runtime":"NeMo-Speech.cpp 0.3.0 CLI",
            "sourceModel":crate::nemo::REPOSITORY,"actualDevice":device,
            "language":language,"detectedLanguages":languages,"quantization":"Q8_0",
            "modelLoadPolicy":"on_demand","modelReused":false,"timestampType":timestamp,
            "audioChunkSeconds":max,"audioChunkMaxSeconds":max,
            "audioChunking":chunking,"audioChunkCount":chunk_count,
            "continuousSpeechMaxSeconds":vad.as_ref().map(|_| NEMO_SPEECH_MAX),
            "nativeStreaming":realtime,"streamingTransport":false,
            "diarizationStatus":gate.status(),
            "diarizationDevice":enabled.then_some("cpu")}))
    }
    .await;
    let _ = tokio::fs::remove_file(path).await;
    let _ = tokio::fs::remove_file(whole).await;
    result
}
/// 処理範囲のWAVをVADで調べ、Nemotron ASRへ渡す塊（範囲内の秒）を返す。
/// 話者区間があれば各発話の話者を求め、話者の切り替わりも切れ目にする。
#[allow(clippy::too_many_arguments)]
async fn nemo_vad_chunks(
    app: &App,
    id: &str,
    vad: &Path,
    whole: &Path,
    start: f64,
    end: f64,
    spans: Option<&[Value]>,
    cancel: &CancellationToken,
) -> Result<Vec<(f64, f64)>> {
    let tools = app.tools();
    let speech = tools.speech.as_ref().context("speech.cppがありません")?;
    let mut args = vec!["vad".into(), vad.to_string_lossy().into()];
    args.extend(owned(["--format", "json", "--speech-pad-ms", "200"]));
    args.extend(owned(["--min-silence-duration-ms", "300"]));
    args.extend(["--threads".into(), whisper_threads(app).to_string()]);
    args.push(whole.to_string_lossy().into());
    let timeout = Duration::from_secs_f64(((end - start) * 2.).max(120.));
    let raw = run_capture(app, speech, &args, cancel, id, timeout).await?;
    let payload: Value = serde_json::from_slice(&raw)?;
    let mut regions = vec![];
    for region in payload["regions"]
        .as_array()
        .context("VADの時刻がありません")?
    {
        let left = region["start"].as_f64().context("VAD開始時刻が不正です")?;
        let right = region["end"].as_f64().context("VAD終了時刻が不正です")?;
        if !left.is_finite()
            || !right.is_finite()
            || left < 0.
            || right <= left
            || right > end - start + 0.01
        {
            bail!("VAD区間が不正です")
        }
        let right = right.min(end - start);
        let speaker = spans.and_then(|spans| dominant_speaker(spans, start + left, start + right));
        regions.push((left, right, speaker));
    }
    regions.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(nemo_chunks(&regions))
}
async fn provider(
    app: &App,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
) -> Result<Value> {
    let cfg = catalog::provider(
        job["engine"].as_str().unwrap(),
        job["modelId"].as_str().unwrap(),
    )?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut offset = start;
    let chunk: f64 = if cfg.format == "json" { 60. } else { 600. };
    let path = app.store.dir.join(id).join("provider.wav");
    let result = async {
        while offset < end {
            let length = chunk.min(end - offset);
            wav(app, id, source, offset, length, &path, cancel).await?;
            let payload = provider_request(&client, &cfg, job, &path, cancel).await?;
            if let Some(segments) = payload["segments"].as_array() {
                // 1行ずつ検証して保存する。不正な行に達したらそれ以前の行を残して失敗する。
                for row in segments {
                    app.store
                        .append(id, provider_segment(row, offset, length, end)?)?;
                }
            } else if let Some(text) = payload["text"].as_str().filter(|s| !s.trim().is_empty()) {
                if cfg.format != "json" {
                    bail!("時刻付きAPI結果にsegmentsがありません")
                }
                app.store.append(
                    id,
                    json!({"text":text,"start":offset,"end":offset+length,
                        "speaker":"","speakerSource":"unassigned","speakerNeedsReview":false,
                        "timestampType":"api_chunk_estimated","transcriptionNeedsReview":true}),
                )?;
            } else {
                bail!("APIの文字起こし結果がありません")
            }
            offset += length;
            app.store.update(id, json!({"processedUntil":offset}))?;
        }
        let diarization = if job["diarizationEnabled"] == true {
            "complete"
        } else {
            "not_performed"
        };
        Ok::<_, anyhow::Error>(json!({"runtime":"Rust provider adapter",
            "diarizationStatus":diarization,"externalApi":true}))
    }
    .await;
    let _ = tokio::fs::remove_file(path).await;
    result
}
/// 分割音声1つを外部APIへ送り、8MBまでの応答JSONを返す。
async fn provider_request(
    client: &reqwest::Client,
    cfg: &catalog::Provider,
    job: &Value,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Value> {
    use futures_util::StreamExt;
    let size = tokio::fs::metadata(path).await?.len();
    if size > 24_000_000 {
        bail!("外部API用の分割音声が大きすぎます")
    }
    let file = tokio::fs::File::open(path).await?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let part = reqwest::multipart::Part::stream_with_length(body, size)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let model = job["modelId"].as_str().unwrap().to_string();
    let mut form = reqwest::multipart::Form::new()
        .text("model", model)
        .text("response_format", cfg.format.clone())
        .part("file", part);
    if !cfg.language.is_empty() {
        let field = if job["modelId"] == "gpt-transcribe" {
            "languages[]"
        } else {
            "language"
        };
        form = form.text(field, cfg.language.clone());
    }
    if cfg.format == "diarized_json" {
        form = form.text("chunking_strategy", "auto");
    }
    let mut request = client.post(&cfg.endpoint).multipart(form);
    if !cfg.key.is_empty() {
        request = request.bearer_auth(&cfg.key)
    }
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("処理を中止しました"),
        response = request.send() => {
            response.map_err(|_| anyhow::anyhow!("文字起こしAPIへ接続できません"))?
        }
    };
    if !response.status().is_success() {
        bail!(
            "文字起こしAPIがHTTP {}を返しました。設定と利用枠を確認してください",
            response.status().as_u16()
        )
    }
    let mut bytes = vec![];
    let mut stream = response.bytes_stream();
    while let Some(data) = tokio::select! {
        _ = cancel.cancelled() => bail!("処理を中止しました"),
        data = stream.next() => data,
    } {
        let data = data.map_err(|_| anyhow::anyhow!("API応答の取得に失敗しました"))?;
        if bytes.len() + data.len() > 8_000_000 {
            bail!("API応答が大きすぎます")
        }
        bytes.extend(data)
    }
    Ok(serde_json::from_slice(&bytes)?)
}
/// 外部APIの時刻付き字幕1行を検証し、元音声の時刻へ直した字幕にする。
fn provider_segment(row: &Value, offset: f64, length: f64, end: f64) -> Result<Value> {
    let left = row["start"].as_f64().context("API字幕時刻がありません")?;
    let right = row["end"].as_f64().context("API字幕時刻がありません")?;
    if !left.is_finite() || !right.is_finite() || left < 0. || right <= left || right > length + 0.1
    {
        bail!("API字幕の時刻が不正です")
    }
    let speaker = row["speaker"].as_str().unwrap_or("");
    let source = if speaker.is_empty() {
        "unassigned"
    } else {
        "automatic"
    };
    let text = row["text"].as_str().context("API字幕がありません")?;
    Ok(
        json!({"text":text,"start":offset+left,"end":(offset+right).min(end),
        "speaker":speaker,"speakerSource":source,"speakerNeedsReview":false,
        "timestampType":"api_segment"}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diarization_spans_keep_overlap_offset_and_reject_invalid_output() {
        let spans=diarization_spans(&json!({"segments":[{"start":1.0,"end":4.0,"speaker":1},{"start":0.25,"end":2.0,"speaker":0}]}),10.,13.).unwrap();
        assert_eq!(spans,json!([{"start":10.25,"end":12.0,"speaker":"話者1"},{"start":11.0,"end":13.0,"speaker":"話者2"}]).as_array().unwrap().clone());
        assert!(
            diarization_spans(&json!({"segments":[]}), 0., 3.)
                .unwrap()
                .is_empty()
        );
        for row in [
            json!({"start":-1,"end":1,"speaker":0}),
            json!({"start":1,"end":1,"speaker":0}),
            json!({"start":0,"end":1,"speaker":8}),
            json!({"start":0,"end":1}),
        ] {
            assert!(diarization_spans(&json!({"segments":[row]}), 0., 3.).is_err());
        }
        assert!(diarization_spans(&json!({}), 0., 3.).is_err());
    }
    #[test]
    fn native_times_are_required_and_not_guessed() {
        let mut p = NativeParser::default();
        assert!(p.feed(&json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"x","transcript":"text"}),0.,true).is_err());
    }
    #[test]
    fn overlap_and_dominance_use_union() {
        assert_eq!(union_duration(vec![(0., 2.), (1., 3.), (5., 6.)]), 4.);
        let mut s = json!({"start":0,"end":4});
        assign_speakers(
            &mut s,
            &[
                json!({"start":0,"end":3,"speaker":"A"}),
                json!({"start":1,"end":2,"speaker":"A"}),
                json!({"start":3,"end":4,"speaker":"B"}),
            ],
        );
        assert_eq!(s["speaker"], "A");
        assert_eq!(s["speakerCandidates"][0]["duration"], 3.);
        assert_eq!(s["speakerCoverage"], 1.);
    }
    #[test]
    fn provider_segments_are_shifted_to_source_time_and_validated() {
        // 600秒の分割の2つ目（offset=600）で、処理範囲の終わりは1000秒。
        let row = json!({"start":1.5,"end":4.0,"text":"本文","speaker":"A"});
        assert_eq!(
            provider_segment(&row, 600., 400., 1000.).unwrap(),
            json!({"text":"本文","start":601.5,"end":604.0,"speaker":"A","speakerSource":"automatic",
                "speakerNeedsReview":false,"timestampType":"api_segment"})
        );
        // 話者なしは未割当。終わりは処理範囲で切り、分割長+0.1秒までの超過は許す。
        let row = json!({"start":399.,"end":400.05,"text":"末尾"});
        let segment = provider_segment(&row, 600., 400., 1000.).unwrap();
        assert_eq!(segment["end"], 1000.);
        assert_eq!(segment["speaker"], "");
        assert_eq!(segment["speakerSource"], "unassigned");
        for row in [
            json!({"start":-0.1,"end":1.,"text":"x"}),
            json!({"start":2.,"end":2.,"text":"x"}),
            json!({"start":0.,"end":400.2,"text":"x"}),
            json!({"start":0.,"end":1.}),
            json!({"end":1.,"text":"x"}),
        ] {
            assert!(provider_segment(&row, 0., 400., 400.).is_err(), "{row}");
        }
    }
    #[test]
    fn whisper_segments_must_stay_in_range_with_bounded_text() {
        let ok = json!({"start":10.,"end":12.,"text":"本文"});
        assert!(check_whisper_segment(&ok, 10., 20.).is_ok());
        assert!(
            check_whisper_segment(&json!({"start":10.,"end":20.0005,"text":""}), 10., 20.).is_ok()
        );
        for segment in [
            json!({"start":9.9,"end":12.,"text":"x"}),
            json!({"start":10.,"end":20.01,"text":"x"}),
            json!({"start":12.,"end":12.,"text":"x"}),
            json!({"start":10.,"end":12.}),
            json!({"start":10.,"end":12.,"text":"あ".repeat(266_667)}),
        ] {
            assert!(check_whisper_segment(&segment, 10., 20.).is_err());
        }
    }
    #[test]
    fn nemo_chunks_join_speech_within_30_seconds_without_silence_between_chunks() {
        // 30秒以内は結合し（塊の中の無音は送る）、超える発話から新しい塊。塊同士の間（29.5〜31秒）は送らない。
        let r = [
            (0., 8., None),
            (9., 20., None),
            (21., 29.5, None),
            (31., 40., None),
            (50., 52., None),
        ];
        assert_eq!(nemo_chunks(&r), vec![(0., 29.5), (31., 52.)]);
        assert!(nemo_chunks(&[]).is_empty());
        // 余白で重なった部分は直前の塊の後ろだけを送る。
        assert_eq!(
            nemo_chunks(&[(0., 10., None), (9.8, 45., None)]),
            vec![(0., 10.), (10., 45.)]
        );
    }
    #[test]
    fn nemo_chunks_split_at_speaker_changes_but_not_short_chunks() {
        let r = [
            (0., 8., Some("話者1")),
            (9., 12., Some("話者2")),
            (13., 14., Some("話者2")),
            (14.5, 15., None),
            (16., 17., Some("話者1")),
            (18., 18.5, Some("話者2")),
            (19., 21., Some("話者2")),
        ];
        // 話者1→2、2→1で切る。話者のない発話（14.5秒）は切り替わりにしない。
        // 16秒からの塊は4秒未満なので、18秒の話者2へ替わっても足す。
        assert_eq!(nemo_chunks(&r), vec![(0., 8.), (9., 15.), (16., 21.)]);
        let short = [(0., 2., Some("話者1")), (2.5, 3.5, Some("話者2"))];
        assert_eq!(nemo_chunks(&short), vec![(0., 3.5)]);
    }
    #[test]
    fn nemo_chunks_keep_continuous_speech_up_to_five_minutes() {
        // 無音のない発話は30秒で切らず、300秒までは1つの塊にする（ユーザー指定）。
        assert_eq!(
            nemo_chunks(&[(1., 200., None), (201., 205., None)]),
            vec![(1., 200.), (201., 205.)]
        );
        assert_eq!(nemo_chunks(&[(0., 300., None)]), vec![(0., 300.)]);
        assert_eq!(
            nemo_chunks(&[(0., 700., None), (701., 710., None)]),
            vec![(0., 300.), (300., 600.), (600., 700.), (701., 710.)]
        );
    }
    #[test]
    fn dominant_speaker_uses_longest_overlap_and_wav_header_is_16k_mono() {
        let spans = [
            json!({"start":10.,"end":12.,"speaker":"話者1"}),
            json!({"start":11.,"end":15.,"speaker":"話者2"}),
        ];
        assert_eq!(dominant_speaker(&spans, 10., 14.), Some("話者2"));
        assert_eq!(dominant_speaker(&spans, 10., 11.2), Some("話者1"));
        assert_eq!(dominant_speaker(&spans, 20., 21.), None);
        let wav = wav_16k(&[1, 0, 2, 0, 3, 0]);
        assert_eq!(wav.len(), 50);
        assert_eq!(wav_data_16k(&wav, 50).unwrap(), (44, 6));
        assert_eq!(&wav[44..], &[1, 0, 2, 0, 3, 0]);
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 42);
    }
    #[test]
    fn ffmpeg_ranges_match_measured_ffmpeg_rounding() {
        // 期待値はffmpeg 8.1.2の-ss/-t出力とバイト一致した実測の換算（マイクロ秒切り捨て→四捨五入）。
        assert_eq!(
            ffmpeg_sample_range(10_000_000, 0.29, 0.57),
            Some((4640, 9120))
        );
        assert_eq!(
            ffmpeg_sample_range(10_000_000, 1.0000314, 2.9999686),
            Some((16000, 47999))
        );
        assert_eq!(
            ffmpeg_sample_range(10_000_000, 5.123456789, 1.0),
            Some((81975, 16000))
        );
        assert_eq!(ffmpeg_sample_range(10_000_000, 0.000032, 0.000031), None);
        assert_eq!(ffmpeg_sample_range(100, 0.005, 1.0), Some((80, 20)));
        assert_eq!(ffmpeg_sample_range(100, 9.0, 1.0), Some((100, 0)));
        assert_eq!(ffmpeg_micros(-1.0), None);
        assert_eq!(ffmpeg_micros(12.0), Some(12_000_000));
    }
    fn wav_header(channels: u16, rate: u32, data: u32) -> Vec<u8> {
        let mut raw = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        raw.extend(16u32.to_le_bytes());
        raw.extend(1u16.to_le_bytes());
        raw.extend(channels.to_le_bytes());
        raw.extend(rate.to_le_bytes());
        raw.extend((rate * 2).to_le_bytes());
        raw.extend(2u16.to_le_bytes());
        raw.extend(16u16.to_le_bytes());
        raw.extend(b"LIST");
        raw.extend(3u32.to_le_bytes());
        raw.extend([1, 2, 3, 0]);
        raw.extend(b"data");
        raw.extend(data.to_le_bytes());
        raw
    }
    #[test]
    fn wav_layout_skips_padded_chunks_and_rejects_other_formats() {
        let header = wav_header(1, 16000, 10);
        let offset = header.len() as u64;
        assert_eq!(wav_data_16k(&header, offset + 10).unwrap(), (offset, 10));
        assert_eq!(wav_data_16k(&header, offset + 7).unwrap(), (offset, 6));
        assert!(wav_data_16k(&wav_header(2, 16000, 10), offset + 10).is_err());
        assert!(wav_data_16k(&wav_header(1, 48000, 10), offset + 10).is_err());
        assert!(wav_data_16k(b"RIFF\0\0\0\0WAVEdata\0\0\0\0", 20).is_err());
    }
    fn store() -> (std::path::PathBuf, crate::store::Store) {
        let path = std::env::temp_dir().join(format!("tc-gate-{}", uuid::Uuid::new_v4()));
        let store = crate::store::Store::open(&path).unwrap();
        (path, store)
    }
    fn running(store: &crate::store::Store) -> String {
        store
            .create(json!({"status":"running","phase":"transcribing","segmentCount":0,"processedUntil":0,"metadata":{}}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .into()
    }
    #[test]
    fn held_results_are_stored_like_serial_diarization() {
        let (path, store) = store();
        let spans = vec![
            json!({"start":0.0,"end":2.5,"speaker":"話者1"}),
            json!({"start":2.5,"end":9.0,"speaker":"話者2"}),
        ];
        let events = [
            Some(json!({"start":0.5,"end":2.0,"text":"一"})),
            None,
            Some(json!({"start":2.2,"end":3.0,"text":"二"})),
            Some(json!({"start":4.0,"end":8.5,"text":"三"})),
        ];
        // 直列: 話者区間が先に確定している。
        let serial_cell = OnceLock::new();
        serial_cell.set(spans.clone()).unwrap();
        let serial = running(&store);
        let mut gate = SpeakerGate::new(true, &serial_cell);
        // 並行: 認識途中で話者区間が確定する。
        let parallel_cell = OnceLock::new();
        let parallel = running(&store);
        let mut late = SpeakerGate::new(true, &parallel_cell);
        for (index, event) in events.iter().enumerate() {
            if index == 2 {
                assert_eq!(store.get(&parallel).unwrap()["segmentCount"], 0);
                assert_eq!(store.get(&parallel).unwrap()["processedUntil"], 0);
                parallel_cell.set(spans.clone()).unwrap();
            }
            for (gate, id) in [(&mut gate, &serial), (&mut late, &parallel)] {
                match event {
                    Some(segment) => gate.segment(&store, id, segment.clone()).unwrap(),
                    None => gate
                        .update(&store, id, json!({"processedUntil":2.1}))
                        .unwrap(),
                }
            }
        }
        gate.recognition_done(&store, &serial).unwrap();
        late.recognition_done(&store, &parallel).unwrap();
        let (a, b) = (
            store.segments(&serial, 0, 10).unwrap(),
            store.segments(&parallel, 0, 10).unwrap(),
        );
        assert_eq!(a["segments"], b["segments"]);
        assert_eq!(a["segments"][1]["speaker"], "話者1 / 話者2");
        assert_eq!(store.get(&serial).unwrap()["processedUntil"], 8.5);
        assert_eq!(store.get(&parallel).unwrap()["processedUntil"], 8.5);
        assert_eq!(late.status(), "complete");
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn gate_reports_diarization_progress_and_passes_through_when_disabled() {
        let (path, store) = store();
        let cell = OnceLock::new();
        let id = running(&store);
        let mut waiting = SpeakerGate::new(true, &cell);
        waiting
            .segment(&store, &id, json!({"start":0,"end":1,"text":"x"}))
            .unwrap();
        waiting.recognition_done(&store, &id).unwrap();
        assert_eq!(store.get(&id).unwrap()["phase"], "diarizing");
        assert_eq!(store.get(&id).unwrap()["segmentCount"], 0);
        cell.set(vec![]).unwrap();
        waiting.flush(&store, &id).unwrap();
        let stored = store.segments(&id, 0, 10).unwrap();
        assert_eq!(stored["segments"][0]["speakerSource"], "unassigned");
        assert_eq!(waiting.status(), "no_speakers");
        let off = OnceLock::new();
        let plain = running(&store);
        let mut gate = SpeakerGate::new(false, &off);
        gate.segment(
            &store,
            &plain,
            json!({"start":0,"end":1,"text":"y","speaker":""}),
        )
        .unwrap();
        assert_eq!(
            store.segments(&plain, 0, 10).unwrap()["segments"][0].get("speakerCandidates"),
            None
        );
        assert_eq!(gate.status(), "not_performed");
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
}
