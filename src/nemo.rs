//! Fixed-model native NeMo ASR. No Python or secondary HTTP server.
use crate::{
    app::App,
    catalog::{self, Tools},
    platform,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub const ENGINE: &str = "nemotron-asr";
pub const MODEL: &str = "nemotron-3.5-asr-streaming-0.6b";
pub const REPOSITORY: &str = "nvidia/nemotron-3.5-asr-streaming-0.6b";
pub const MODELS: [(&str, &str); 1] = [(MODEL, "Nemotron 3.5 ASR Streaming 0.6B")];
pub const FILENAME: &str = "nemotron-3.5-asr-streaming-0.6b-3fc991d3.q8_0.gguf";
pub const SIZE: u64 = 742_090_464;
pub fn configure(command: &mut tokio::process::Command, exe: &Path) {
    if let Some(parent) = exe.parent() {
        for index in [
            parent.join("share/nemo-speech/model-index.json"),
            parent.join("../share/nemo-speech/model-index.json"),
        ] {
            if index.is_file() {
                command.env("NEMO_SPEECH_MODEL_INDEX", index);
                break;
            }
        }
    }
}
const SHA256: &str = "3fc991d3badad7277c11030a7519832cddaf2057aafed6d4b25147e953a070b1";
const URL: &str = "https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b/resolve/ea30d66debe3740a08b573244286791d423d6b3e/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf";
struct PartialDownload(PathBuf);
impl Drop for PartialDownload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub(crate) const LANGUAGES: [&str; 33] = [
    "auto", "en-US", "en-GB", "es-US", "es-ES", "fr-FR", "fr-CA", "it-IT", "pt-BR", "pt-PT",
    "nl-NL", "de-DE", "tr-TR", "ru-RU", "ar-AR", "hi-IN", "ja-JP", "ko-KR", "vi-VN", "uk-UA",
    "pl-PL", "sv-SE", "cs-CZ", "nb-NO", "da-DK", "bg-BG", "fi-FI", "hr-HR", "sk-SK", "zh-CN",
    "hu-HU", "ro-RO", "et-EE",
];
pub fn language(value: Option<&str>) -> Result<&str> {
    let value = match value.unwrap_or("ja-JP") {
        "ja" => "ja-JP",
        "en" => "en-US",
        v => v,
    };
    if !LANGUAGES.contains(&value) {
        bail!("Nemotron ASRの言語は対応locale（ja-JPなど）またはautoを指定してください")
    };
    Ok(value)
}
pub fn installed(tools: &Tools) -> bool {
    std::fs::metadata(&tools.nemo_model).is_ok_and(|m| m.is_file() && m.len() == SIZE)
}
pub fn require_model(tools: &Tools) -> Result<&Path> {
    if !installed(tools) {
        bail!("UNAVAILABLE: Nemotron ASRのモデルを管理画面からインストールしてください")
    }
    Ok(&tools.nemo_model)
}
pub async fn discover(tools: &Tools, job: &platform::Job, media: bool, diarization: bool) -> Value {
    let mut doctor = json!({});
    let mut supported = false;
    let mut runtime_healthy = false;
    if let Some(exe) = &tools.nemo {
        doctor = tools
            .probe_json(exe, &["--json".into(), "doctor".into()], job)
            .await
            .unwrap_or(json!({}));
        let index = tools
            .probe_json(exe, &["--json".into(), "model".into(), "list".into()], job)
            .await
            .unwrap_or(json!({}));
        runtime_healthy = doctor["features"].is_object() && index["models"].is_array();
        supported = doctor["features"]["asr"] == true
            && index["models"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r["repo"] == REPOSITORY));
    }
    let gpu =
        doctor["accelerator_available"] == true && doctor["features"]["backend_vulkan"] == true;
    let ready = media && supported && installed(tools);
    let availability = if !supported {
        "unavailable"
    } else if installed(tools) {
        "installed"
    } else {
        "download_on_first_use"
    };
    let reason = if !media {
        "FFmpeg/FFprobeがありません"
    } else if !supported {
        "Nemotron 3.5対応のNeMo-Speech.cppを準備してください"
    } else if !installed(tools) {
        "Nemotron ASRのモデルを管理画面からインストールしてください"
    } else {
        ""
    };
    let model = json!({"id":MODEL,"label":MODELS[0].1,"availability":availability,
        "timestampType":"nemo_alignment","supportedLanguages":LANGUAGES.as_slice(),
        "diarization":diarization,
        "diarizationModel":diarization.then_some("nemotron-3-diarization"),
        "diarizationLabel":diarization.then_some("Nemotron 3 Diarization")});
    json!({"id":ENGINE,"label":"Nemotron ASR · 多言語","ready":ready,"task":"transcription",
        "runtime":"NeMo-Speech.cpp CLI","reason":reason,"diarization":diarization,
        "integratedDiarization":false,"nativeStreaming":true,"streamingTransport":false,
        "realtimeMode":"bounded_jobs","runtimeHealthy":runtime_healthy,
        "defaultProcessingMethod":"auto","automaticDevice":if gpu{"gpu"}else{"cpu"},
        "automaticFallback":"cpu","processingMethods":catalog::methods(gpu,false),
        "modelInstallRequired":true,"models":[model]})
}
pub async fn install(app: &App, cancel: &CancellationToken) -> Result<()> {
    if installed(&app.tools()) {
        return Ok(());
    }
    let target = &app.tools().nemo_model;
    let parent = target.parent().context("モデル保存先が不正です")?;
    tokio::fs::create_dir_all(parent).await?;
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".install.lock"))?;
    fs2::FileExt::try_lock_exclusive(&guard).context("他のサーバーが同じモデルを導入中です")?;
    if installed(&app.tools()) {
        return Ok(());
    }
    if fs2::available_space(parent)? < SIZE + 128 * 1024 * 1024 {
        bail!("モデル保存先の空き容量が不足しています")
    }
    let part = parent.join(format!("{}.download", uuid::Uuid::new_v4().simple()));
    // Also cleans up when the management task drops this future on cancellation.
    let _partial = PartialDownload(part.clone());
    let result = async {
        use futures_util::StreamExt;
        // HTTPSの転送先だけを最大5回までたどる。
        let redirect = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" && attempt.previous().len() < 6 {
                attempt.follow()
            } else {
                attempt.error("モデル取得の転送先が不正です")
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(900))
            .redirect(redirect)
            .build()?;
        let response = tokio::select! {
            _ = cancel.cancelled() => bail!("取得を中止しました"),
            r = client.get(URL).send() => r?,
        }
        .error_for_status()?;
        if response.content_length().is_some_and(|n| n != SIZE) {
            bail!("モデルの配布サイズが一致しません")
        }
        let mut out = tokio::fs::File::create(&part).await?;
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => bail!("取得を中止しました"),
            r = stream.next() => r,
        } {
            let chunk = chunk?;
            count += chunk.len() as u64;
            if count > SIZE {
                bail!("モデルの容量上限を超えました")
            }
            out.write_all(&chunk).await?;
            hash.update(&chunk);
            let mut state = app.management.lock().unwrap();
            state["downloadedBytes"] = count.into();
            state["totalBytes"] = SIZE.into();
        }
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        if count != SIZE || hex::encode(hash.finalize()) != SHA256 {
            bail!("モデルのサイズまたはSHA256が一致しません")
        }
        if cancel.is_cancelled() {
            bail!("取得を中止しました")
        }
        tokio::fs::rename(&part, target).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

/// JSON alignment units may be phrases in Japanese. Preserve their returned times.
pub fn segments(payload: &Value, offset: f64, end: f64) -> Result<Vec<Value>> {
    let text = payload["text"]
        .as_str()
        .context("Nemotron ASRの本文が不正です")?;
    let words = payload["words"]
        .as_array()
        .context("Nemotron ASRの時刻情報が不正です")?;
    let mut result = vec![];
    let mut last = 0.;
    for word in words {
        let body = word["word"]
            .as_str()
            .context("Nemotron ASRの語が不正です")?;
        let left = word["start"]
            .as_f64()
            .context("Nemotron ASRの開始時刻が不正です")?;
        let right = word["end"]
            .as_f64()
            .context("Nemotron ASRの終了時刻が不正です")?;
        if !left.is_finite()
            || !right.is_finite()
            || left < last
            || right < left
            || left < 0.
            || right > end - offset + 0.1
        {
            bail!("Nemotron ASRの時刻が処理範囲を超えています")
        }
        last = left;
        if body.trim().is_empty() {
            continue;
        }
        let estimated = right == left;
        let (start, finish) = if estimated {
            (offset, end)
        } else {
            (offset + left, (offset + right).min(end))
        };
        if finish <= start {
            bail!("Nemotron ASRの区間が不正です")
        }
        result.push(json!({"start":start,"end":finish,"text":body,"speaker":"","speakerSource":"unassigned","speakerNeedsReview":false,
            "timestampType":if estimated{"nemo_chunk_estimated"}else{"nemo_alignment"},"transcriptionNeedsReview":estimated}));
    }
    let aligned = result
        .iter()
        .filter_map(|r| r["text"].as_str())
        .collect::<String>();
    let normalized = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if !text.trim().is_empty() && (result.is_empty() || normalized(&aligned) != normalized(text)) {
        result.clear();
        result.push(json!({"start":offset,"end":end,"text":text,"speaker":"","speakerSource":"unassigned","speakerNeedsReview":false,
            "timestampType":"nemo_chunk_estimated","transcriptionNeedsReview":true}));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locales_and_model_identity_are_fixed() {
        assert_eq!(language(None).unwrap(), "ja-JP");
        assert_eq!(language(Some("ja")).unwrap(), "ja-JP");
        assert_eq!(language(Some("auto")).unwrap(), "auto");
        assert!(language(Some("../model")).is_err());
        assert!(language(Some("el-GR")).is_err());
    }
    #[test]
    fn alignment_offset_silence_and_missing_times_are_explicit() {
        let rows = segments(
            &json!({"text":"日本語","words":[{"word":"日本語","start":0.5,"end":2.0}]}),
            10.,
            13.,
        )
        .unwrap();
        assert_eq!(rows[0]["start"], 10.5);
        assert_eq!(rows[0]["end"], 12.0);
        assert_eq!(rows[0]["timestampType"], "nemo_alignment");
        assert!(
            segments(&json!({"text":"","words":[]}), 0., 2.)
                .unwrap()
                .is_empty()
        );
        let missing = segments(&json!({"text":"本文を保持","words":[]}), 0., 2.).unwrap();
        assert_eq!(missing[0]["text"], "本文を保持");
        assert_eq!(missing[0]["transcriptionNeedsReview"], true);
        let incomplete = segments(
            &json!({"text":"本文を全部保持","words":[{"word":"本文","start":0.5,"end":1.0}]}),
            0.,
            2.,
        )
        .unwrap();
        assert_eq!(incomplete[0]["text"], "本文を全部保持");
        assert_eq!(incomplete[0]["timestampType"], "nemo_chunk_estimated");
        let zero = segments(
            &json!({"text":"ゼロ","words":[{"word":"ゼロ","start":1.,"end":1.}]}),
            0.,
            2.,
        )
        .unwrap();
        assert_eq!(zero[0]["timestampType"], "nemo_chunk_estimated");
        for row in [
            json!({"word":"bad","start":-1,"end":2}),
            json!({"word":"bad","start":0,"end":5}),
        ] {
            assert!(segments(&json!({"text":"bad","words":[row]}), 0., 2.).is_err());
        }
    }
}
