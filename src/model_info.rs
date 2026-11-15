//! Public, read-only model metadata. No paths, tokens, jobs, or transcripts.
use crate::app::App;
use serde_json::{Value, json};
use std::sync::OnceLock;

fn metadata() -> &'static Value {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../dist/model-metadata.json"))
            .expect("validated model metadata")
    })
}

fn usage(model: &Value) -> &'static str {
    match model["engine"].as_str().unwrap_or("") {
        "nemotron-transcribe" => "integrated",
        "auxiliary" if model["id"] == "auxiliary/silero-vad" => "vad",
        "auxiliary" => "diarization",
        _ => "transcription",
    }
}

fn describe(model: &Value) -> Value {
    let data = metadata();
    let id = model["id"].as_str().unwrap_or("");
    let mut row = json!({});
    // Deliberate allowlist: management status can contain local diagnostics/paths.
    for key in [
        "id",
        "engine",
        "engineLabel",
        "label",
        "modelInstalled",
        "runtimeReady",
        "available",
        "supported",
        "supportStatus",
        "availability",
        "managementState",
        "loadPolicy",
        "processingMethods",
        "runtimeId",
        "missingRuntimes",
    ] {
        row[key] = model[key].clone();
    }
    row["usage"] = usage(model).into();
    row["capabilities"] = crate::recognition::capabilities(model["engine"].as_str().unwrap_or(""));
    row["guidance"] = data["modelGuidance"][id].clone();
    row["referenceSpeed"] = Value::Null;
    row["referenceFiller"] = Value::Null;
    if let Some(sample) = data["referenceBenchmarks"]["models"].get(id) {
        let mut speed = sample.clone();
        // Each run's ratio was calculated before taking the median. Dividing the
        // median character count by the median duration produces a different value.
        speed.as_object_mut().unwrap().remove("filler");
        speed["unit"] = "characters/s".into();
        speed["kind"] = "historical_measurement".into();
        for key in ["cpu", "device", "definition", "caution"] {
            speed[key] = data["referenceBenchmarks"][key].clone();
        }
        row["referenceSpeed"] = speed;
        let mut filler = sample["filler"].clone();
        for key in ["referenceDate", "repetitions", "condition", "serverVersion"] {
            filler[key] = sample[key].clone();
        }
        filler["definition"] = data["referenceBenchmarks"]["fillerDefinition"].clone();
        filler["caution"] = data["referenceBenchmarks"]["fillerCaution"].clone();
        row["referenceFiller"] = filler;
    }
    row
}

pub fn inventory(app: &App) -> Value {
    let engines = app.engines.lock().unwrap().clone();
    let models = app.model_inventory.lock().unwrap();
    let rows: Vec<_> = models
        .as_array()
        .into_iter()
        .flatten()
        .map(|model| {
            let mut row = describe(model);
            row["capabilities"] = crate::recognition::runtime_capabilities(
                model["engine"].as_str().unwrap_or(""),
                &engines,
            );
            row["execution"] =
                crate::scheduler::model_counts(app, model["id"].as_str().unwrap_or(""));
            row
        })
        .collect();
    let mut environment = metadata()["referenceBenchmarks"].clone();
    environment.as_object_mut().unwrap().remove("models");
    json!({
        "schemaVersion": 1, "serverVersion": crate::config::VERSION,
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "models": rows, "groups": metadata()["groups"],
        "referenceEnvironment": environment,
        "note": "保有と実行可否は別です。参考速度は記載CPU・版・条件での保存済み測定であり、この接続先PCの現在速度を保証しません。フィラー候補数は出力傾向で、認識精度や原音忠実性ではありません。"
    })
}

pub fn enrich(app: &App, catalog: &mut Value) {
    let engines = app.engines.lock().unwrap().clone();
    let models = app.model_inventory.lock().unwrap();
    for entry in catalog["data"].as_array_mut().into_iter().flatten() {
        entry["x_meeting"]["capabilities"] = crate::recognition::runtime_capabilities(
            entry["x_meeting"]["engineId"].as_str().unwrap_or(""),
            &engines,
        );
        if let Some(model) = models
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["id"] == entry["id"])
        {
            entry["x_meeting"]["modelInfo"] = describe(model);
            entry["x_meeting"]["modelInfo"]["capabilities"] =
                entry["x_meeting"]["capabilities"].clone();
            entry["x_meeting"]["modelInfo"]["execution"] =
                crate::scheduler::model_counts(app, model["id"].as_str().unwrap_or(""));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_covers_all_managed_models_and_preserves_reference_conditions() {
        let data = metadata();
        let mut ids = vec![];
        for (id, guidance) in data["modelGuidance"].as_object().unwrap() {
            assert!(guidance["bytes"].as_u64().unwrap() > 0);
            for key in ["time", "quality", "source", "ram", "vram"] {
                assert!(!guidance[key].as_str().unwrap().is_empty());
            }
            ids.push(id.as_str());
        }
        assert_eq!(ids.len(), 15);
        for (engine, _, models) in crate::catalog::NATIVE {
            for (id, _) in models {
                assert!(ids.contains(&format!("{engine}/{id}").as_str()));
            }
        }
        for (id, _) in crate::catalog::WHISPER_MODELS {
            assert!(ids.contains(&format!("whisper/{id}").as_str()));
        }
        let tiny = describe(
            &json!({"id":"whisper/tiny", "engine":"whisper", "modelInstalled":false,"available":false,"token":"private","dataDirectory":"private","statusMessage":"private"}),
        );
        assert_eq!(tiny["modelInstalled"], false);
        assert_eq!(tiny["available"], false);
        assert_eq!(
            data["referenceBenchmarks"]["models"]
                .as_object()
                .unwrap()
                .len(),
            12
        );
        assert_eq!(tiny["referenceSpeed"]["serverVersion"], "0.1.10");
        assert!(
            (tiny["referenceSpeed"]["charactersPerSecond"]
                .as_f64()
                .unwrap()
                - 17.35259966455941)
                .abs()
                < 0.001
        );
        assert_eq!(tiny["referenceSpeed"]["kind"], "historical_measurement");
        assert_eq!(tiny["referenceFiller"]["candidatesMedian"], 2);
        assert_eq!(tiny["referenceFiller"]["audioVerified"], false);
        assert!(!tiny.to_string().contains("private"));
        let base = describe(&json!({"id":"whisper/base","engine":"whisper"}));
        assert_eq!(base["referenceFiller"]["candidatesMin"], 0);
        let small = describe(&json!({"id":"whisper/small","engine":"whisper"}));
        assert_eq!(small["referenceFiller"]["candidatesMedian"], 0);
        let nemo = describe(
            &json!({"id":"nemotron-asr/nemotron-3.5-asr-streaming-0.6b","engine":"nemotron-asr"}),
        );
        assert_eq!(nemo["referenceSpeed"]["loadIncludedInProcessing"], true);
        assert!(nemo["referenceSpeed"]["loadReadySeconds"].is_null());
        for id in [
            "auxiliary/silero-vad",
            "nemotron-transcribe/nemotron-3.5-transcribe",
        ] {
            let model = describe(&json!({"id":id}));
            assert!(model["referenceSpeed"].is_null());
            assert!(model["referenceFiller"].is_null());
        }
        assert_eq!(
            usage(&json!({"id":"auxiliary/silero-vad","engine":"auxiliary"})),
            "vad"
        );
    }
}
