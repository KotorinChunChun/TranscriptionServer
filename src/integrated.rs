//! Nemotron Transcribe: local C++ worker, with its own speaker/timestamp output.
use crate::{app::App, catalog, jobs, models::NativeSession, platform};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub const ENGINE: &str = "nemotron-transcribe";
pub const MODEL: &str = "nemotron-3.5-transcribe";
pub const MODELS: &[(&str, &str)] = &[(MODEL, "Nemotron 3.5 Transcribe 30B-A4B")];
pub const MODEL_BYTES: u64 = 18_997_225_856;
pub const DISABLED_REASON: &str =
    "約19GBの一体型モデルは現在の設定で無効です。明示的に有効化した環境だけで導入・実行します。";
pub fn check_enabled(app: &App) -> Result<()> {
    if !app.runtime.config.enable_integrated_model {
        bail!("UNAVAILABLE: {DISABLED_REASON}")
    }
    Ok(())
}

pub async fn require_model(tools: &catalog::Tools, scope: &platform::Job) -> Result<PathBuf> {
    let exe = tools
        .speech
        .as_ref()
        .context("UNAVAILABLE: speech.cppが未導入です")?;
    let index = tools
        .probe_json(exe, &["models".into(), "--json".into()], scope)
        .await?;
    catalog::installed_model_file(&index, MODEL, "recognition")
        .context("UNAVAILABLE: Nemotron一体型（IQ4_NL・約19GB）を管理画面から先に導入してください。認識時には自動取得しません")
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    app: &App,
    lane: usize,
    id: &str,
    job: &Value,
    source: &Path,
    start: f64,
    end: f64,
    cancel: &CancellationToken,
) -> Result<Value> {
    check_enabled(app)?;
    if end - start > 3600. {
        bail!(
            "Nemotron一体型は1回の処理範囲を1時間以内にしてください。話者番号を保つため自動分割しません"
        )
    }
    let model = require_model(&app.tools(), &app.supervisor).await?;
    let path = app.store.dir.join(id).join("integrated-input.wav");
    let result = async {
        jobs::wav(app, id, source, start, end - start, &path, cancel).await?;
        let mut audio = jobs::PcmReader::open(&path).await?;
        let pcm = audio
            .read(0., end - start)
            .await?
            .context("一体型モデルへ渡す音声が短すぎます")?;
        let expected = format!("{ENGINE}/{MODEL}");
        let mut resident = app.native_slots[lane].lock().await;
        let reused = resident
            .as_ref()
            .is_some_and(|s| s.id == expected && s.device == "cpu");
        if !reused {
            if let Some(s) = resident.as_mut() {
                s.unload().await?;
            }
            *resident = None;
        }
        let mut temporary;
        let session = if reused {
            resident.as_mut().unwrap()
        } else {
            let model = model.to_str().context("モデルの保存パスが不正です")?;
            temporary = NativeSession::load(app, &expected, model, "cpu", cancel, Some(id)).await?;
            &mut temporary
        };
        let scope = session.scope();
        app.store.update(
            id,
            json!({"phase":"transcribing","message":"Nemotron一体型で文字起こししています。",
                "metadata":{"modelReused":reused,"actualDevice":"cpu"}}),
        )?;
        let speakers = job["diarizationEnabled"] == true;
        let recognize = async {
            tokio::time::timeout(
                Duration::from_secs(86400),
                session.transcribe_integrated(
                    &pcm,
                    speakers,
                    crate::recognition::for_job(job),
                    cancel,
                ),
            )
            .await
            .context("一体型モデルの認識がタイムアウトしました")?
        };
        let label = "Nemotron一体型の認識・話者付与";
        let answer = crate::telemetry::measure(
            &app.store,
            id,
            "recognition",
            label,
            cancel,
            Some(&scope),
            recognize,
        )
        .await?;
        // 字幕の時刻合わせに失敗しても、時刻なし本文とモデルの詳細出力は残す。
        let saved = app.store.dir.join(id).join("integrated-result.json");
        tokio::fs::write(saved, serde_json::to_vec_pretty(&answer)?).await?;
        app.store.update(
            id,
            json!({"metadata":{"integratedText":answer["text"],"alignmentError":answer["untimed"],
                "integratedResultSaved":true}}),
        )?;
        let segments = parse_segments(&answer, start, end, speakers)?;
        let has_speakers = segments
            .iter()
            .any(|s| s["speaker"].as_str().is_some_and(|s| !s.is_empty()));
        for segment in segments {
            if cancel.is_cancelled() {
                bail!("処理を中止しました")
            }
            app.store.append(id, segment)?;
        }
        let status = if !speakers {
            "not_performed"
        } else if has_speakers {
            "complete"
        } else {
            "no_speakers"
        };
        Ok(
            json!({"runtime":"speech.cpp worker protocol4","actualDevice":"cpu",
            "modelReused":reused,"quantization":"IQ4_NL","timestampType":"word_aligned_segment",
            "diarizationModel":speakers.then_some(MODEL),"diarizationStatus":status,
            "transcriptionNeedsReview":answer["stop"]=="model_limit"}),
        )
    }
    .await;
    let _ = tokio::fs::remove_file(path).await;
    if result.is_err() {
        let mut resident = app.native_slots[lane].lock().await;
        if let Some(session) = resident.as_mut() {
            session.unload().await?;
        }
        *resident = None;
    }
    result
}

fn parse_segments(answer: &Value, start: f64, end: f64, speakers: bool) -> Result<Vec<Value>> {
    let text = answer["text"]
        .as_str()
        .context("一体型モデルの本文がありません")?;
    if !matches!(answer["stop"].as_str(), Some("complete" | "model_limit")) {
        bail!("一体型モデルの終了状態が不正です")
    }
    if answer["untimed"].as_str().is_some_and(|s| !s.is_empty()) {
        bail!(
            "一体型モデルの時刻を確定できませんでした。本文と理由をintegrated-result.jsonへ保存しました"
        )
    }
    let rows = match answer["segments"].as_array() {
        Some(rows) => rows,
        None if text.trim().is_empty() => return Ok(vec![]),
        None => bail!("一体型モデルの字幕時刻がありません。本文は保存しました"),
    };
    if rows.is_empty() && !text.trim().is_empty() {
        bail!("一体型モデルの本文に対応する字幕時刻がありません")
    }
    rows.iter()
        .map(|row| {
            let left = row["start"]
                .as_f64()
                .context("一体型の開始時刻が不正です")?;
            let right = row["end"].as_f64().context("一体型の終了時刻が不正です")?;
            if !left.is_finite()
                || !right.is_finite()
                || left < 0.
                || right <= left
                || right > end - start + 0.1
            {
                bail!("一体型モデルの字幕時刻が処理範囲と一致しません")
            }
            let text = row["text"]
                .as_str()
                .context("一体型モデルの字幕本文がありません")?;
            let speaker = if speakers && !row["speaker"].is_null() {
                let number = row["speaker"]
                    .as_u64()
                    .filter(|n| *n < 1000)
                    .context("一体型モデルの話者番号が不正です")?;
                format!("話者{}", number + 1)
            } else {
                String::new()
            };
            let source = if speaker.is_empty() {
                "unassigned"
            } else {
                "automatic"
            };
            Ok(
                json!({"start":start+left,"end":(start+right).min(end),"text":text,
            "speakerSource":source,"speakerNeedsReview":speakers&&speaker.is_empty(),
            "speaker":speaker,"timestampType":"word_aligned_segment",
            "transcriptionNeedsReview":answer["stop"]=="model_limit"}),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn integrated_preserves_overlap_offsets_speakers_and_rejects_bad_alignment() {
        let answer = json!({"text":"はい。了解。","segments":[{"start":0.,"end":2.,"text":"はい。","speaker":0},{"start":1.,"end":3.,"text":"了解。","speaker":1}],"stop":"complete"});
        let rows = parse_segments(&answer, 10., 13., true).unwrap();
        assert_eq!(rows[0]["start"], 10.);
        assert_eq!(rows[1]["start"], 11.);
        assert_eq!(rows[1]["speaker"], "話者2");
        assert_eq!(
            parse_segments(&answer, 10., 13., false).unwrap()[1]["speaker"],
            ""
        );
        for mut value in [
            json!({"text":"本文","untimed":"alignment_failed"}),
            json!({"text":"本文"}),
            json!({"text":"本文","segments":[]}),
            json!({"text":"本文","segments":[{"start":0.,"end":4.,"text":"本文"}]}),
            json!({"text":"本文","segments":[{"start":0.,"end":1.,"text":"本文","speaker":-1}]}),
        ] {
            value["stop"] = "complete".into();
            assert!(parse_segments(&value, 10., 13., true).is_err());
        }
        assert!(
            parse_segments(
                &json!({"text":"","segments":[],"stop":"complete"}),
                0.,
                1.,
                false
            )
            .unwrap()
            .is_empty()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn integrated_fixture_checks_cpu_protocol_reuse_untimed_and_missing_weights() {
        use crate::{config, models};
        let dir = std::env::temp_dir().join(format!("tc-integrated-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fixture.exe");
        let csc = PathBuf::from(std::env::var_os("WINDIR").unwrap())
            .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
        let output = platform::command(&csc)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([
                "/nologo",
                "/r:System.Web.Extensions.dll",
                &format!("/out:{}", exe.display()),
                "tests\\fixtures\\integrated-runtime.cs",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        std::fs::write(dir.join("model.gguf"), b"12345678").unwrap();
        let app = App::new(config::Runtime {
            config: config::Config {
                instance_id: "integrated-fixture".into(),
                enable_integrated_model: true,
                data_dir: dir.join("data"),
                speech: Some(exe.clone()),
                ffmpeg: Some(exe.clone()),
                ffprobe: Some(exe),
                whisper_worker: Some(dir.join("missing.exe")),
                ..Default::default()
            },
            root: dir.clone(),
            home: dir.join("settings"),
            installed: false,
            process_record: None,
            open: false,
        })
        .await
        .unwrap();
        {
            let mut scheduler = app.scheduler.lock().unwrap();
            scheduler.test_memory = Some((131072, 131072));
            scheduler.settings.vram_limit_mib = 65536;
        }
        let engines = app.engines.lock().unwrap().clone();
        let engine = engines
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == ENGINE)
            .unwrap();
        assert_eq!(engine["ready"], true); // No separate VAD or diarization model.
        assert!(
            !engine["processingMethods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == "gpu")
        );
        let record = app
            .store
            .create(json!({"status":"running","rangeStart":2.,"rangeEnd":5.,"metadata":{}}))
            .unwrap();
        let id = record["id"].as_str().unwrap().to_owned();
        let source = dir.join("source.wav");
        std::fs::write(&source, b"fixture").unwrap();
        let job = json!({"engine":ENGINE,"modelId":MODEL,"diarizationEnabled":true});
        let stop = CancellationToken::new();
        let first = run(&app, 0, &id, &job, &source, 2., 5., &stop)
            .await
            .unwrap();
        assert_eq!(first["modelReused"], false);
        assert!(app.native_slots[0].lock().await.is_none());
        let page = app.store.segments(&id, 0, 100).unwrap();
        let rows = page["segments"].as_array().unwrap();
        assert_eq!(rows[0]["start"], 2.25);
        assert_eq!(rows[1]["speaker"], "話者2");
        let path = require_model(&app.tools(), &app.supervisor).await.unwrap();
        *app.native_slots[0].lock().await = Some(
            NativeSession::load(
                &app,
                &format!("{ENGINE}/{MODEL}"),
                path.to_str().unwrap(),
                "cpu",
                &stop,
                None,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            run(&app, 0, &id, &job, &source, 2., 5., &stop)
                .await
                .unwrap()["modelReused"],
            true
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("loads.txt"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        std::fs::write(dir.join("mode.txt"), "untimed").unwrap();
        assert!(
            run(&app, 0, &id, &job, &source, 2., 5., &stop)
                .await
                .is_err()
        );
        assert!(app.native_slots[0].lock().await.is_none());
        assert_eq!(
            app.store.get(&id).unwrap()["metadata"]["integratedText"],
            "saved untimed text"
        );
        assert!(
            app.store
                .dir
                .join(&id)
                .join("integrated-result.json")
                .is_file()
        );
        for lane in 0..2 {
            *app.native_slots[lane].lock().await = Some(
                NativeSession::load(
                    &app,
                    &format!("{ENGINE}/{MODEL}"),
                    path.to_str().unwrap(),
                    "cpu",
                    &stop,
                    None,
                )
                .await
                .unwrap(),
            );
        }
        app.store
            .update(&id, json!({"status":"complete","phase":"complete"}))
            .unwrap();
        models::action(
            app.clone(),
            json!({"action":"unload","model":format!("{ENGINE}/{MODEL}"),"lane":1}),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while app.management.lock().unwrap()["state"] == "running" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.management.lock().unwrap()["state"], "complete");
        assert!(app.native_slots[0].lock().await.is_some());
        assert!(app.native_slots[1].lock().await.is_none());
        *app.native_slots[0].lock().await = None;
        std::fs::remove_file(dir.join("model.gguf")).unwrap();
        let query = [
            ("engine".to_string(), ENGINE.to_string()),
            ("model".to_string(), MODEL.to_string()),
        ]
        .into_iter()
        .collect();
        assert!(
            app.create(&query)
                .await
                .unwrap_err()
                .to_string()
                .contains("自動取得しません")
        );
        assert_eq!(app.store.list(None, None).unwrap().len(), 1);
        assert!(
            require_model(&app.tools(), &app.supervisor)
                .await
                .unwrap_err()
                .to_string()
                .contains("自動取得しません")
        );
        assert!(models::selection(&format!("{ENGINE}/{MODEL}")).is_ok());
        app.runtime_pending_restart
            .lock()
            .unwrap()
            .insert("speech".into());
        models::refresh(&app).await;
        let inventory = crate::runtime_install::inventory(&app);
        let speech = inventory
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "speech")
            .unwrap();
        assert_eq!(speech["state"], "restart_required");
        assert!(
            app.selected(ENGINE, MODEL, "cpu", false)
                .unwrap_err()
                .to_string()
                .contains("再起動")
        );
        app.runtime_pending_restart.lock().unwrap().clear();
        std::fs::write(dir.join("broken-runtime.flag"), b"broken").unwrap();
        models::refresh(&app).await;
        let inventory = crate::runtime_install::inventory(&app);
        let speech = inventory
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "speech")
            .unwrap();
        assert_eq!(speech["state"], "runtime_error");
        assert_eq!(speech["installable"], false); // Explicit external fixture is never replaced.
        let status = models::status(&app).await.unwrap();
        let row = status["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["engine"] == ENGINE)
            .unwrap();
        assert_eq!(row["managementState"], "runtime_error");
        app.stop.cancel();
        drop(app);
        // Background queue tasks release their SQLite handle after cancellation.
        for _ in 0..50 {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return,
                Err(e)
                    if matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
                        || e.raw_os_error() == Some(32) =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(e) => panic!("fixture cleanup: {e}"),
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
