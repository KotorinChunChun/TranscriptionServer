//! Shared request options. Persisted per job, never as worker-wide defaults.
use anyhow::Result;
use serde_json::{Value, json};
use std::{collections::HashMap, fmt};

const QWEN_LANGUAGES: &str =
    "ar cs da de el en es fa fi fil fr hi hu id it ja ko mk ms nl pl pt ro ru sv th tr vi yue zh";
const WHISPER_LANGUAGES: &str = "af am ar as az ba be bg bn bo br bs ca cs cy da de el en es et eu fa fi fo fr gl gu ha haw he hi hr ht hu hy id is it ja jw ka kk km kn ko la lb ln lo lt lv mg mi mk ml mn mr ms mt my ne nl nn no oc pa pl ps pt ro ru sa sd si sk sl sn so sq sr su sv sw ta te tg th tk tl tr tt uk ur uz vi yi yo zh yue";

#[derive(Clone, Debug)]
pub struct InputError {
    pub param: String,
    pub code: &'static str,
    pub message: String,
}
impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.param, self.message)
    }
}
impl std::error::Error for InputError {}
pub fn invalid(param: &str, message: &str) -> anyhow::Error {
    InputError {
        param: param.into(),
        code: "invalid_value",
        message: message.into(),
    }
    .into()
}
pub fn unsupported(param: &str) -> anyhow::Error {
    InputError {
        param: param.into(),
        code: "unsupported_parameter",
        message: "このモデルまたは実行経路では未対応です。".into(),
    }
    .into()
}

pub fn capabilities(engine: &str) -> Value {
    let languages: Vec<_> = match engine {
        "whisper" => WHISPER_LANGUAGES.split_whitespace().collect(),
        "qwen" => QWEN_LANGUAGES.split_whitespace().collect(),
        crate::nemo::ENGINE => crate::nemo::LANGUAGES.to_vec(),
        "auxiliary" | "diarization" => vec![],
        _ => vec!["ja"],
    };
    json!({"schemaVersion":1,"languages":languages,
        "automaticLanguage":matches!(engine,"whisper"|"qwen"|crate::nemo::ENGINE),
        "prompt":matches!(engine,"whisper"|"qwen"|crate::integrated::ENGINE),
        "keywords":engine=="haas","keywordsFormat":"array_of_strings",
        "decoding":if engine=="reazon" {json!(["beam","greedy"])} else {json!([])},
        "temperature":engine=="whisper",
        "stream":!matches!(engine,"auxiliary"|"diarization"),"streamFormats":["json"],
        "realtime":{"websocket":matches!(engine,"whisper"|"haas"|"qwen"|"reazon"|"parakeet"|crate::nemo::ENGINE),
            "turnDetection":[null],"inputFormat":{"type":"audio/pcm","rate":24000},"maxCommitSeconds":30},
        "wordTimestamps":false,
        "chunkingStrategies":["auto"],"optionScope":"per_request"})
}

pub fn runtime_capabilities(engine: &str, engines: &Value) -> Value {
    let mut caps = capabilities(engine);
    if engine == "whisper" {
        let options = engines
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["id"] == engine)
            .and_then(|e| e["requestOptions"].as_array());
        for key in ["prompt", "temperature"] {
            caps[key] = options.is_some_and(|v| v.iter().any(|v| v == key)).into();
        }
        if !options.is_some_and(|v| v.iter().any(|v| v == "language")) {
            caps["languages"] = json!(["ja"]);
            caps["automaticLanguage"] = false.into();
        }
    }
    if matches!(engine, "openai" | "compatible") {
        let model = engines
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["id"] == engine)
            .and_then(|e| e["models"].as_array())
            .and_then(|rows| rows.first())
            .and_then(|m| m["id"].as_str())
            .unwrap_or("");
        if let Ok(cfg) = crate::catalog::provider(engine, model) {
            caps["languages"] = if cfg.language.is_empty() {
                json!([])
            } else {
                json!([cfg.language])
            };
            caps["automaticLanguage"] = cfg.language.is_empty().into();
        }
    }
    caps
}
pub fn validate(engine: &str, query: &HashMap<String, String>) -> Result<Value> {
    let caps = capabilities(engine);
    let mut options = json!({});
    let value = query.get("language").map(String::as_str).unwrap_or("ja");
    let language = if engine == crate::nemo::ENGINE {
        crate::nemo::language(Some(value))
            .map_err(|_| invalid("language", "対応localeまたはautoを指定してください。"))?
            .to_owned()
    } else {
        let code = value.split('-').next().unwrap_or("");
        if value.len() > 16
            || value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
        {
            return Err(invalid(
                "language",
                "対応する言語コードを指定してください。",
            ));
        }
        let codes = caps["languages"].as_array().unwrap();
        if value == "auto" && !codes.is_empty() {
            if caps["automaticLanguage"] == true {
                "auto".to_owned()
            } else {
                codes[0].as_str().unwrap().to_owned()
            }
        } else if codes.iter().any(|v| v == code) {
            code.to_owned()
        } else if matches!(engine, "auxiliary" | "diarization") && value == "ja" {
            "auto".to_owned()
        } else {
            return Err(invalid("language", "モデルが対応していない言語です。"));
        }
    };
    options["language"] = language.into();
    if let Some(prompt) = query.get("prompt").filter(|v| !v.is_empty()) {
        if caps["prompt"] != true {
            return Err(unsupported("prompt"));
        }
        if prompt.len() > 8192 || prompt.contains('\0') {
            return Err(invalid(
                "prompt",
                "UTF-8で8192バイト以下を指定してください。",
            ));
        }
        options["prompt"] = prompt.clone().into();
    }
    if let Some(raw) = query.get("keywords") {
        if caps["keywords"] != true {
            return Err(unsupported("keywords"));
        }
        let words: Vec<String> = serde_json::from_str(raw)
            .map_err(|_| invalid("keywords", "文字列のJSON配列を指定してください。"))?;
        if raw.len() > 8192
            || words.len() > 64
            || words
                .iter()
                .any(|s| s.trim().is_empty() || s.contains(['\n', '\r', '\0']))
        {
            return Err(invalid(
                "keywords",
                "改行を含まない語句を64件・合計8192バイト以下で指定してください。",
            ));
        }
        if !words.is_empty() {
            options["keywords"] = words.join("\n").into();
        }
    }
    if let Some(value) = query.get("decoding") {
        if engine != "reazon" {
            return Err(unsupported("decoding"));
        }
        if !matches!(value.as_str(), "beam" | "greedy") {
            return Err(invalid("decoding", "beamまたはgreedyを指定してください。"));
        }
        options["decoding"] = value.clone().into();
    }
    if let Some(raw) = query.get("temperature") {
        let value: f64 = raw
            .parse()
            .map_err(|_| invalid("temperature", "0以上1以下の数値を指定してください。"))?;
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(invalid(
                "temperature",
                "0以上1以下の数値を指定してください。",
            ));
        }
        if engine == "whisper" {
            options["temperature"] = value.into();
        } else if value != 0.0 {
            return Err(unsupported("temperature"));
        }
    }
    Ok(options)
}

pub fn for_job(job: &Value) -> Value {
    if job["metadata"]["recognitionOptions"].is_object() {
        job["metadata"]["recognitionOptions"].clone()
    } else {
        json!({"language":job["metadata"]["language"].as_str().unwrap_or("ja")})
    }
}
pub fn cli_args(job: &Value) -> Vec<String> {
    let options = for_job(job);
    let mut args = vec![];
    for key in ["language", "prompt", "keywords", "decoding"] {
        if let Some(value) = options[key].as_str() {
            args.extend([format!("--{key}"), value.into()]);
        }
    }
    args
}

/// A requested language is not an automatic-detection result.
pub fn response_language(job: &Value) -> Option<String> {
    let detected: Vec<_> = job["metadata"]["detectedLanguages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !detected.is_empty() {
        return Some(detected.join(","));
    }
    let language = job["metadata"]["language"].as_str()?;
    (language != "auto").then(|| language.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn q(items: &[(&str, &str)]) -> HashMap<String, String> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    #[test]
    fn model_specific_options_fail_before_admission_and_survive_persistence() {
        let opts = validate(
            "qwen",
            &q(&[("language", "en-US"), ("prompt", "TranscriptCutter")]),
        )
        .unwrap();
        assert_eq!(opts["language"], "en");
        let job = json!({"metadata":{"recognitionOptions":opts}});
        assert_eq!(
            cli_args(&job),
            vec!["--language", "en", "--prompt", "TranscriptCutter"]
        );
        let restored: Value = serde_json::from_str(&job.to_string()).unwrap();
        assert_eq!(for_job(&restored), for_job(&job));
        assert_eq!(
            validate("haas", &q(&[("keywords", r#"["Haas=ハース","AIモデル"]"#)])).unwrap()["keywords"],
            "Haas=ハース\nAIモデル"
        );
        for (engine, key, value) in [
            ("haas", "prompt", "x"),
            ("qwen", "keywords", "[]"),
            ("haas", "language", "en"),
            ("whisper", "temperature", "NaN"),
            ("haas", "keywords", r#"["x\ny"]"#),
            ("qwen", "decoding", "greedy"),
        ] {
            let error = validate(engine, &q(&[(key, value)])).unwrap_err();
            assert_eq!(error.downcast_ref::<InputError>().unwrap().param, key);
        }
        assert!(response_language(&json!({"metadata":{"language":"auto"}})).is_none());
        assert_eq!(
            response_language(&json!({"metadata":{"language":"auto","detectedLanguages":["en"]}}))
                .unwrap(),
            "en"
        );
        assert_eq!(capabilities("qwen")["wordTimestamps"], false);
    }
}
