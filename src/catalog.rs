use crate::{
    config::{Runtime, program},
    platform,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;
pub const WHISPER_MODELS: [(&str, &str); 6] = [
    ("tiny", "Whisper Tiny"),
    ("base", "Whisper Base"),
    ("small", "Whisper Small"),
    ("medium", "Whisper Medium"),
    ("large-v3-turbo", "Whisper Large v3 Turbo"),
    ("large-v3", "Whisper Large v3"),
];
type NativeEngine = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);
#[cfg(all(test, windows))]
#[test]
fn native_profile_uses_exact_windows_extended_prefix() {
    let tools = Tools {
        environment: vec![(
            "LOCALAPPDATA".into(),
            r"C:\isolated profile/engine-profile/LocalAppData".into(),
        )],
        ffmpeg: None,
        ffprobe: None,
        speech: None,
        nemo: None,
        nemo_model: PathBuf::new(),
        whisper: None,
    };
    let command = tools.command(Path::new("speech.exe"));
    let actual = command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == "LOCALAPPDATA")
        .unwrap()
        .1
        .unwrap();
    assert_eq!(
        actual,
        r"\\?\C:\isolated profile\engine-profile\LocalAppData"
    );
}
pub const NATIVE: [NativeEngine; 5] = [
    (
        crate::integrated::ENGINE,
        "Nemotron一体型",
        crate::integrated::MODELS,
    ),
    ("haas", "Haas", &[("haas-ja-v1.0", "Haas-ja-v1.0")]),
    (
        "qwen",
        "Qwen3-ASR",
        &[
            ("qwen3-asr-0.6b", "Qwen3-ASR 0.6B"),
            ("qwen3-asr-1.7b", "Qwen3-ASR 1.7B"),
        ],
    ),
    (
        "reazon",
        "ReazonSpeech",
        &[("reazonspeech-v2", "ReazonSpeech v2")],
    ),
    (
        "parakeet",
        "Parakeet 日本語",
        &[("parakeet-tdt_ctc-0.6b-ja", "Parakeet TDT/CTC 0.6B 日本語")],
    ),
];
#[derive(Clone)]
pub struct Tools {
    pub environment: Vec<(String, String)>,
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub speech: Option<PathBuf>,
    pub nemo: Option<PathBuf>,
    pub nemo_model: PathBuf,
    pub whisper: Option<(PathBuf, Vec<String>)>,
}
impl Tools {
    pub fn resolve(runtime: &Runtime) -> Self {
        let cfg = &runtime.config;
        let root = &runtime.root;
        let worker = cfg
            .whisper_worker
            .clone()
            .unwrap_or_else(|| root.join("worker/WhisperWorker.exe"));
        let whisper = if worker.is_file() && worker.extension().is_some_and(|v| v != "py") {
            Some((worker, vec![]))
        } else if let Some(python) =
            crate::runtime_install::managed_program(&runtime.home, "whisper", "python")
        {
            // Keep the installed Python/dependencies, but use this server's protocol adapter.
            // An older managed package must not silently ignore new request options.
            managed_whisper_adapter(&runtime.home).ok().map(|script| {
                (
                    python,
                    vec![
                        script.to_string_lossy().into_owned(),
                        "--model-dir".into(),
                        runtime
                            .home
                            .join("models/whisper")
                            .to_string_lossy()
                            .into_owned(),
                    ],
                )
            })
        } else if (runtime.installed || !cfg!(debug_assertions) || cfg.instance_id != "shared")
            && cfg.whisper_worker.is_none()
        {
            None
        } else {
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let source = cfg
                .whisper_worker
                .clone()
                .unwrap_or_else(|| base.join("workers/whisper_worker.py"));
            let python = cfg
                .whisper_python
                .clone()
                .unwrap_or_else(|| base.join("runtime/whisper-python/Scripts/python.exe"));
            if source.is_file() && python.is_file() {
                Some((python, vec![source.to_string_lossy().into_owned()]))
            } else {
                None
            }
        };
        let environment = if cfg.instance_id == "shared" {
            vec![]
        } else {
            [
                ("LOCALAPPDATA", "engine-profile/LocalAppData"),
                ("APPDATA", "engine-profile/Roaming"),
                ("XDG_CACHE_HOME", "engine-cache"),
                ("HF_HOME", "models/huggingface"),
                ("HF_HUB_CACHE", "models/huggingface/hub"),
                ("HUGGINGFACE_HUB_CACHE", "models/huggingface/hub"),
            ]
            .into_iter()
            .map(|(key, path)| {
                (
                    key.into(),
                    runtime.home.join(path).to_string_lossy().into_owned(),
                )
            })
            .chain(std::iter::once((
                "HF_HUB_DISABLE_IMPLICIT_TOKEN".into(),
                "1".into(),
            )))
            .collect()
        };
        Self {
            environment,
            ffmpeg: runtime_program(&cfg.ffmpeg, runtime, "ffmpeg", "ffmpeg"),
            ffprobe: runtime_program(&cfg.ffprobe, runtime, "ffmpeg", "ffprobe"),
            speech: runtime_program(&cfg.speech, runtime, "speech", "speech"),
            nemo: runtime_program(&cfg.nemo_speech, runtime, "nemo", "nemo-speech"),
            nemo_model: runtime
                .home
                .join("models/nemotron-asr")
                .join(crate::nemo::FILENAME),
            whisper,
        }
    }
    pub fn command(&self, program: &Path) -> tokio::process::Command {
        let mut command = platform::command(program);
        command.envs(self.environment.iter().map(|(k, v)| (k, v)));
        #[cfg(windows)]
        if program.file_stem().is_some_and(|s| s == "speech") {
            for name in ["LOCALAPPDATA", "APPDATA"] {
                let value = self
                    .environment
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var(name).ok());
                if let Some(value) = value
                    && Path::new(&value).is_absolute()
                    && !value.starts_with("\\\\?\\")
                {
                    let value = value.replace('/', "\\");
                    let extended = if let Some(unc) = value.strip_prefix("\\\\") {
                        format!("\\\\?\\UNC\\{unc}")
                    } else {
                        format!("\\\\?\\{value}")
                    };
                    command.env(name, extended);
                }
            }
        }
        command
    }
    pub async fn probe_json(
        &self,
        program: &Path,
        args: &[String],
        job: &platform::Job,
    ) -> Result<Value> {
        probe_command(self.command(program), program, args, job).await
    }
}
fn managed_whisper_adapter(home: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let bytes = include_bytes!("../workers/whisper_worker.py");
    let hash = hex::encode(Sha256::digest(bytes));
    let dir = home.join("worker-adapters");
    let target = dir.join(format!("whisper-{hash}.py"));
    if std::fs::read(&target).is_ok_and(|v| v == bytes) {
        return Ok(target);
    }
    std::fs::create_dir_all(&dir)?;
    let stage = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&stage, bytes)?;
    if let Err(error) = std::fs::rename(&stage, &target) {
        let _ = std::fs::remove_file(&stage);
        if !std::fs::read(&target).is_ok_and(|v| v == bytes) {
            return Err(error.into());
        }
    }
    Ok(target)
}
#[cfg(test)]
#[test]
fn managed_adapter_is_current_and_does_not_replace_runtime_package() {
    let dir = std::env::temp_dir().join(format!("tc-adapter-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("whisper_worker.py");
    std::fs::write(&old, b"old runtime adapter").unwrap();
    let first = managed_whisper_adapter(&dir).unwrap();
    assert_eq!(
        std::fs::read(&first).unwrap(),
        include_bytes!("../workers/whisper_worker.py")
    );
    assert_eq!(managed_whisper_adapter(&dir).unwrap(), first);
    assert_eq!(std::fs::read(&old).unwrap(), b"old runtime adapter");
    std::fs::remove_dir_all(dir).unwrap();
}
fn runtime_program(
    explicit: &Option<PathBuf>,
    runtime: &Runtime,
    package: &str,
    name: &str,
) -> Option<PathBuf> {
    if explicit.as_ref().is_some_and(|p| p.is_file()) {
        return explicit.clone();
    }
    crate::runtime_install::managed_program(&runtime.home, package, name).or_else(|| {
        (runtime.config.instance_id == "shared")
            .then(|| program(explicit, &runtime.root, name))
            .flatten()
    })
}
async fn probe_command(
    mut cmd: tokio::process::Command,
    program: &Path,
    args: &[String],
    job: &platform::Job,
) -> Result<Value> {
    if program.file_stem().is_some_and(|s| s == "nemo-speech") {
        crate::nemo::configure(&mut cmd, program);
    }
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("SPEECH_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1");
    let mut child = cmd.spawn()?;
    job.assign(&child)?;
    let stdout = child.stdout.take().context("probe stdout")?;
    let mut bytes = vec![];
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        stdout
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        let status = child.wait().await?;
        if !status.success() || bytes.len() > 2 * 1024 * 1024 {
            bail!("モデル点検に失敗しました")
        };
        Ok::<Value, anyhow::Error>(serde_json::from_slice(&bytes)?)
    })
    .await;
    match result {
        Ok(v) => v,
        Err(_) => {
            let _ = child.kill().await;
            bail!("モデル点検がタイムアウトしました")
        }
    }
}
pub(crate) fn methods(gpu: bool, api: bool) -> Value {
    if api {
        return json!([{"id":"auto","label":"API側で処理"}]);
    }
    let mut rows = vec![
        json!({"id":"auto","label":"自動"}),
        json!({"id":"cpu","label":"CPU"}),
    ];
    if gpu {
        rows.push(json!({"id":"gpu","label":"GPU候補"}))
    }
    json!(rows)
}
fn model(id: &str, label: &str, diar: bool, availability: &str, timestamp: &str) -> Value {
    json!({"id":id,"label":label,"diarization":diar,
        "diarizationModel":diar.then_some("nemotron-3-diarization"),
        "diarizationLabel":diar.then_some("Nemotron 3 Diarization"),
        "availability":availability,"timestampType":timestamp})
}
pub fn installed_diarization_file(native: &Value) -> Option<PathBuf> {
    installed_model_file(native, "nemotron-3-diarization", "diarization")
}
/// 導入済みのsilero-vad重み。未導入なら自動取得せずNoneを返す。
pub async fn installed_vad_file(tools: &Tools, job: &platform::Job) -> Option<PathBuf> {
    let speech = tools.speech.as_ref()?;
    let native = tools
        .probe_json(speech, &["models".into(), "--json".into()], job)
        .await
        .ok()?;
    installed_model_file(&native, "silero-vad", "detection")
}
/// speech.cppのモデル一覧から、取得済みで大きさが一致する単一の重みファイルを返す。
pub(crate) fn installed_model_file(native: &Value, name: &str, task: &str) -> Option<PathBuf> {
    let row = native["models"]
        .as_array()?
        .iter()
        .find(|row| row["name"] == name && row["task"] == task)?;
    let weights: Vec<_> = row["files"]
        .as_array()?
        .iter()
        .filter(|file| file["type"] == row["type"])
        .collect();
    if weights.len() != 1 || weights[0]["fetched"] != true {
        return None;
    }
    let path = PathBuf::from(weights[0]["path"].as_str()?);
    let metadata = std::fs::metadata(&path).ok()?;
    (metadata.is_file()
        && metadata.len() > 0
        && Some(metadata.len()) == weights[0]["size"].as_u64())
    .then_some(path)
}
pub async fn require_diarization_file(tools: &Tools, job: &platform::Job) -> Result<PathBuf> {
    let speech = tools
        .speech
        .as_ref()
        .context("UNAVAILABLE: speech.cppが未導入です")?;
    let native = tools
        .probe_json(speech, &["models".into(), "--json".into()], job)
        .await?;
    installed_diarization_file(&native)
        .with_context(|| format!("UNAVAILABLE: {DIARIZATION_MISSING}"))
}
const DIARIZATION_MISSING: &str =
    "Nemotron 3 Diarizationの重みが未導入です。管理画面から導入して再点検してください";
pub async fn discover(tools: &Tools, job: &platform::Job) -> Value {
    let native = if let Some(speech) = &tools.speech {
        tools
            .probe_json(speech, &["models".into(), "--json".into()], job)
            .await
            .unwrap_or(json!({}))
    } else {
        json!({})
    };
    let devices = if let Some(speech) = &tools.speech {
        tools
            .probe_json(speech, &["devices".into(), "--json".into()], job)
            .await
            .unwrap_or(json!({}))
    } else {
        json!({})
    };
    let rows = native["models"].as_array().cloned().unwrap_or_default();
    let native_gpu = devices["devices"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|r| matches!(r["kind"].as_str(), Some("gpu" | "igpu")))
    });
    let available =
        |id: &str, task: &str| rows.iter().find(|r| r["name"] == id && r["task"] == task);
    let diar = available("nemotron-3-diarization", "diarization").is_some();
    let media = tools.ffmpeg.is_some() && tools.ffprobe.is_some();
    let mut engines = vec![];
    let whisper = if let Some((exe, args)) = &tools.whisper {
        let mut args = args.clone();
        args.push("--probe".into());
        tools.probe_json(exe, &args, job).await.unwrap_or(json!({}))
    } else {
        json!({})
    };
    let whisper_models = WHISPER_MODELS
        .iter()
        .filter(|(id, _)| {
            whisper["models"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v == *id))
        })
        .map(|(id, label)| {
            model(
                id,
                label,
                diar,
                whisper["availability"][*id]
                    .as_str()
                    .unwrap_or("download_on_first_use"),
                "whisper_segment",
            )
        })
        .collect::<Vec<_>>();
    let reason = if !media {
        "FFmpeg/FFprobeがありません"
    } else if whisper_models.is_empty() {
        "Whisper専用ワーカーを点検できません"
    } else {
        ""
    };
    engines.push(json!({"id":"whisper","label":"Whisper",
        "requestOptions":whisper["requestOptions"],
        "ready":media&&!whisper_models.is_empty(),"reason":reason,
        "runtime":"faster-whisper worker","gpuComputeType":whisper["gpuComputeType"],
        "gpuNote":whisper["gpuNote"],"diarization":diar,
        "processingMethods":methods(whisper["gpu"]==true,false),
        "defaultProcessingMethod":"auto","models":whisper_models}));
    for (engine, label, models) in NATIVE {
        let integrated = engine == crate::integrated::ENGINE;
        let selected = models
            .iter()
            .filter_map(|(id, label)| {
                let row = available(id, "recognition")?;
                let files = row["files"].as_array().cloned().unwrap_or_default();
                let weights = files
                    .iter()
                    .filter(|f| f["type"] == row["type"])
                    .collect::<Vec<_>>();
                let installed = !weights.is_empty() && weights.iter().all(|f| f["fetched"] == true);
                let mut entry = model(
                    id,
                    label,
                    diar || integrated,
                    if installed {
                        "installed"
                    } else if integrated {
                        "manual_install_required"
                    } else {
                        "download_on_first_use"
                    },
                    if integrated {
                        "word_aligned_segment"
                    } else {
                        "vad_utterance"
                    },
                );
                if integrated {
                    entry["diarizationModel"] = id.to_string().into();
                    entry["diarizationLabel"] = "Nemotron一体型（内蔵）".into();
                    entry["downloadBytes"] = crate::integrated::MODEL_BYTES.into();
                    entry["manualInstallRequired"] = true.into();
                }
                Some(entry)
            })
            .collect::<Vec<_>>();
        let ready = media
            && !selected.is_empty()
            && (integrated || available("silero-vad", "detection").is_some());
        let reason = if ready {
            ""
        } else if integrated {
            "speech.cpp/FFmpeg/FFprobe/対応モデルを点検してください"
        } else {
            "speech.cpp/FFmpeg/FFprobe/対応モデル/VADを点検してください"
        };
        engines.push(
            json!({"id":engine,"label":label,"ready":ready,"reason":reason,
            "runtime":"speech.cpp","diarization":diar||integrated,
            "processingMethods":methods(native_gpu&&!integrated,false),
            "defaultProcessingMethod":"auto","models":selected}),
        );
    }
    engines.push(crate::nemo::discover(tools, job, media, diar).await);
    let diarization_installed = installed_diarization_file(&native).is_some();
    let standalone_ready = media && diarization_installed;
    let mut standalone = model(
        "nemotron-3-diarization",
        "Nemotron 3 Diarization",
        true,
        if diarization_installed {
            "installed"
        } else {
            "download_on_first_use"
        },
        "diarization_segment",
    );
    standalone["task"] = "diarization".into();
    let reason = if !media {
        "FFmpeg/FFprobeがありません"
    } else if tools.speech.is_none() {
        "speech.cppが未導入です"
    } else if !diarization_installed {
        DIARIZATION_MISSING
    } else {
        ""
    };
    engines.push(
        json!({"id":"diarization","label":"話者分離専用","task":"diarization",
        "ready":standalone_ready,"reason":reason,"runtime":"speech.cpp","diarization":true,
        "processingMethods":methods(native_gpu,false),"defaultProcessingMethod":"auto",
        "models":[standalone]}),
    );
    for (engine, label) in [("openai", "OpenAI API"), ("compatible", "OpenAI 互換 API")] {
        let descriptors = provider_models(engine);
        let ready = media
            && provider(
                engine,
                descriptors
                    .first()
                    .and_then(|m| m["id"].as_str())
                    .unwrap_or(""),
            )
            .is_ok();
        let reason = if ready {
            ""
        } else {
            "提供先のAPI設定またはFFmpeg/FFprobeが未準備です"
        };
        engines.push(
            json!({"id":engine,"label":label,"ready":ready,"reason":reason,
            "provider":engine,"externalApi":true,"processingMethods":methods(false,true),
            "defaultProcessingMethod":"auto","models":descriptors}),
        );
    }
    json!(engines)
}
pub fn catalog(engines: &Value) -> Value {
    let mut rows = vec![];
    for engine in engines
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["ready"] == true)
    {
        for m in engine["models"].as_array().into_iter().flatten() {
            let id = format!(
                "{}/{}",
                engine["id"].as_str().unwrap(),
                m["id"].as_str().unwrap()
            );
            let meeting = json!({"engineId":engine["id"],"engineLabel":engine["label"],
                "modelId":m["id"],"label":m["label"],
                "processingMethods":engine["processingMethods"],"defaultProcessingMethod":"auto",
                "task":engine["task"].as_str().unwrap_or("transcription"),
                "diarization":m["diarization"],"diarizationModel":m["diarizationModel"],
                "diarizationLabel":m["diarizationLabel"],"timestampType":m["timestampType"],
                "availability":m["availability"]});
            let mut row = json!({"id":id,"object":"model","created":0,
                "owned_by":"meeting-transcriber","x_meeting":meeting});
            for key in [
                "nativeStreaming",
                "streamingTransport",
                "realtimeMode",
                "integratedDiarization",
                "modelInstallRequired",
            ] {
                if let Some(value) = engine.get(key) {
                    row["x_meeting"][key] = value.clone();
                }
            }
            for key in ["manualInstallRequired", "downloadBytes"] {
                if let Some(value) = m.get(key) {
                    row["x_meeting"][key] = value.clone();
                }
            }
            if let Some(languages) = m.get("supportedLanguages") {
                row["x_meeting"]["supportedLanguages"] = languages.clone();
            }
            let external = engine["externalApi"] == true;
            row["x_meeting"]["supported"] = true.into();
            row["x_meeting"]["supportStatus"] = "confirmed".into();
            row["x_meeting"]["runtimeReady"] = true.into();
            row["x_meeting"]["runtimeInstallable"] = false.into();
            row["x_meeting"]["modelInstalled"] = if external {
                Value::Null
            } else {
                (m["availability"] == "installed").into()
            };
            row["x_meeting"]["available"] = (external || m["availability"] == "installed").into();
            rows.push(row)
        }
    }
    json!({"object":"list","data":rows})
}
pub struct Provider {
    pub endpoint: String,
    pub key: String,
    pub format: String,
    pub language: String,
}
pub fn provider_models(engine: &str) -> Vec<Value> {
    if engine == "openai" {
        [
            (
                "gpt-transcribe",
                "GPT Transcribe",
                false,
                "api_chunk_estimated",
            ),
            (
                "gpt-4o-transcribe",
                "GPT-4o Transcribe",
                false,
                "api_chunk_estimated",
            ),
            (
                "gpt-4o-mini-transcribe",
                "GPT-4o mini Transcribe",
                false,
                "api_chunk_estimated",
            ),
            (
                "gpt-4o-transcribe-diarize",
                "GPT-4o 話者分離",
                true,
                "api_segment",
            ),
            ("whisper-1", "Whisper API", false, "api_segment"),
        ]
        .iter()
        .map(|(id, label, d, t)| {
            let mut m = model(id, label, *d, "provider", t);
            m["diarizationModel"] = if *d { (*id).into() } else { Value::Null };
            m
        })
        .collect()
    } else {
        let id = std::env::var("TRANSCRIPTION_API_MODEL").unwrap_or_default();
        if id.is_empty() {
            return vec![];
        }
        let diar =
            std::env::var("TRANSCRIPTION_API_RESPONSE_FORMAT").is_ok_and(|f| f == "diarized_json");
        vec![model(
            &id,
            &id,
            diar,
            "provider",
            if std::env::var("TRANSCRIPTION_API_RESPONSE_FORMAT").unwrap_or("json".into()) == "json"
            {
                "api_chunk_estimated"
            } else {
                "api_segment"
            },
        )]
    }
}
pub fn provider(engine: &str, model: &str) -> Result<Provider> {
    let cfg = if engine == "openai" {
        if !provider_models(engine).iter().any(|m| m["id"] == model) {
            bail!("APIモデルを確認してください")
        };
        Provider {
            endpoint: "https://api.openai.com/v1/audio/transcriptions".into(),
            key: std::env::var("OPENAI_API_KEY").context("OpenAI APIは未設定です")?,
            format: if model == "gpt-4o-transcribe-diarize" {
                "diarized_json"
            } else if model == "whisper-1" {
                "verbose_json"
            } else {
                "json"
            }
            .into(),
            language: "ja".into(),
        }
    } else {
        if std::env::var("TRANSCRIPTION_API_MODEL").unwrap_or_default() != model || model.is_empty()
        {
            bail!("互換APIモデルを確認してください")
        };
        let base = std::env::var("TRANSCRIPTION_API_BASE_URL")?;
        let mut url = url::Url::parse(&base)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
        {
            bail!("互換APIはHTTPS（localhostのみHTTP可）を指定してください")
        };
        url.set_path(&format!(
            "{}/audio/transcriptions",
            url.path().trim_end_matches('/')
        ));
        Provider {
            endpoint: url.to_string(),
            key: std::env::var("TRANSCRIPTION_API_KEY").unwrap_or_default(),
            format: std::env::var("TRANSCRIPTION_API_RESPONSE_FORMAT").unwrap_or("json".into()),
            language: std::env::var("TRANSCRIPTION_API_LANGUAGE").unwrap_or("ja".into()),
        }
    };
    if (engine == "openai" && cfg.key.is_empty())
        || !cfg.key.bytes().all(|b| (33..=126).contains(&b))
        || !matches!(
            cfg.format.as_str(),
            "json" | "verbose_json" | "diarized_json"
        )
        || (!cfg.language.is_empty()
            && (!(2..=3).contains(&cfg.language.len())
                || !cfg.language.bytes().all(|b| b.is_ascii_lowercase())))
    {
        bail!("APIの認証/形式/言語設定を確認してください")
    };
    Ok(cfg)
}
