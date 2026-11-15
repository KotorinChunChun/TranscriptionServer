//! Transport adapters over the existing persisted jobs and resource scheduler.
use crate::{app::App, http::ApiError, recognition, store::active};
use anyhow::{Context, Result, bail};
use axum::{
    body::Body,
    extract::{
        FromRequestParts,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{Request, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Semaphore, mpsc},
};

const MAX_PCM: usize = 24_000 * 2 * 30;
const MAX_TEXT: usize = 8 * 1024 * 1024;
static WS_SESSIONS: Semaphore = Semaphore::const_new(32);
fn append(pcm: &mut Vec<u8>, raw: &str) -> Result<()> {
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|_| recognition::invalid("audio", "base64音声が不正です。"))?;
    if chunk.len() % 2 != 0 || pcm.len() + chunk.len() > MAX_PCM {
        return Err(recognition::invalid(
            "audio",
            "PCM16音声を最大30秒以内で確定してください。",
        ));
    }
    pcm.extend(chunk);
    Ok(())
}

struct OwnedJobs {
    app: Arc<App>,
    ids: HashSet<String>,
}
impl OwnedJobs {
    fn new(app: Arc<App>) -> Self {
        Self {
            app,
            ids: HashSet::new(),
        }
    }
}
impl Drop for OwnedJobs {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = self.app.cancel(id);
        }
    }
}
struct Cursor {
    app: Arc<App>,
    id: String,
    generation: Value,
    after: u64,
    text: String,
}
struct Batch {
    deltas: Vec<String>,
    done: bool,
}
impl Cursor {
    fn new(app: Arc<App>, id: &str) -> Result<Self> {
        let generation = app.store.get(id)?["metadata"]["resultGeneration"].clone();
        Ok(Self {
            app,
            id: id.into(),
            generation,
            after: 0,
            text: String::new(),
        })
    }
    fn poll(&mut self) -> Result<Batch> {
        let job = self.app.store.get(&self.id)?;
        if job["metadata"]["resultGeneration"] != self.generation {
            bail!(
                "認識が再実行され結果世代が変わりました。旧字幕に連結せずジョブを再取得してください。"
            )
        }
        if !active(&job) && job["status"] != "complete" {
            bail!(
                "{}",
                job["error"]
                    .as_str()
                    .unwrap_or("認識が中止または失敗しました。")
            )
        }
        let page = self.app.store.segments(&self.id, self.after, 200)?;
        let rows = page["segments"].as_array().context("字幕一覧が不正です")?;
        let mut deltas = vec![];
        for row in rows {
            let text = row["text"].as_str().unwrap_or("");
            let delta = format!("{}{}", if self.text.is_empty() { "" } else { "\n" }, text);
            if self.text.len() + delta.len() > MAX_TEXT {
                bail!("逐次通知の本文上限を超えました。")
            }
            self.text.push_str(&delta);
            deltas.push(delta);
        }
        self.after = page["next"].as_u64().unwrap_or(self.after);
        Ok(Batch {
            deltas,
            done: job["status"] == "complete" && rows.len() < 200,
        })
    }
}
async fn send<T>(tx: &mpsc::Sender<T>, value: T) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), tx.send(value))
        .await
        .context("受信側への通知がタイムアウトしました")?
        .map_err(|_| anyhow::anyhow!("接続が閉じられました"))
}
pub fn sse(app: Arc<App>, id: &str) -> Result<Response> {
    let mut cursor = Cursor::new(app.clone(), id)?;
    let (tx, rx) = mpsc::channel(16);
    let mut owned = OwnedJobs::new(app);
    owned.ids.insert(id.into());
    let job_id = id.to_owned();
    tokio::spawn(async move {
        let _owned = owned;
        loop {
            if tx.is_closed() {
                break;
            }
            let result = cursor.poll();
            match result {
                Ok(batch) => {
                    for delta in batch.deltas {
                        if send(&tx, json!({"type":"transcript.text.delta","delta":delta}))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    if batch.done {
                        let _ = send(
                            &tx,
                            json!({"type":"transcript.text.done","text":cursor.text,
                            "x_meeting":{"jobId":cursor.id,"resultGeneration":cursor.generation}}),
                        )
                        .await;
                        break;
                    }
                }
                Err(error) => {
                    let _=send(&tx,json!({"type":"error","error":{"type":"server_error","code":"transcription_failed","message":error.to_string()},"x_meeting":{"jobId":cursor.id}})).await;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|value| {
            let event = Event::default()
                .event(value["type"].as_str().unwrap_or("error"))
                .data(value.to_string());
            (Ok::<_, Infallible>(event), rx)
        })
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response();
    response
        .headers_mut()
        .insert("x-meeting-job-id", job_id.parse()?);
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse()?);
    Ok(response)
}

pub async fn upgrade(
    app: Arc<App>,
    request: Request<Body>,
) -> std::result::Result<Response, ApiError> {
    let permit = WS_SESSIONS.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "リアルタイム接続数の上限です。".into(),
        )
    })?;
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let model = query.get("model").cloned();
    if query.get("intent").is_some_and(|v| v != "transcription") {
        return Err(recognition::invalid("intent", "transcriptionを指定してください。").into());
    }
    let (mut parts, _) = request.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "WebSocket接続を指定してください。".into(),
            )
        })?;
    Ok(ws
        .protocols(["realtime"])
        .max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            if let Err(error) = session(app, socket, model).await {
                eprintln!("リアルタイム接続を終了しました: {error}");
            }
        })
        .into_response())
}
fn event(mut value: Value) -> Value {
    value["event_id"] = format!("event_{}", uuid::Uuid::new_v4().simple()).into();
    value
}
fn error_event(error: &anyhow::Error, cause: &Value) -> Value {
    let input = error.downcast_ref::<recognition::InputError>();
    event(
        json!({"type":"error","error":{"type":"invalid_request_error",
        "code":input.map(|v|v.code).unwrap_or("invalid_request"),
        "param":input.map(|v|&v.param),"message":error.to_string(),"event_id":cause["event_id"]}}),
    )
}
fn check_keys(value: &Value, allowed: &[&str], param: &str) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| recognition::invalid(param, "オブジェクトを指定してください。"))?;
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(recognition::unsupported(param));
    }
    Ok(())
}
fn update(config: &mut HashMap<String, String>, value: &Value) -> Result<()> {
    check_keys(
        value,
        &[
            "type",
            "audio",
            "input_audio_format",
            "input_audio_transcription",
            "turn_detection",
            "x_meeting",
        ],
        "session",
    )?;
    if value.get("type").is_some_and(|v| v != "transcription") {
        return Err(recognition::unsupported("session.type"));
    }
    if let Some(extra) = value.get("x_meeting") {
        check_keys(extra, &["device"], "x_meeting")?;
        if let Some(device) = extra.get("device") {
            let device = device
                .as_str()
                .filter(|v| matches!(*v, "auto" | "cpu" | "gpu"))
                .ok_or_else(|| {
                    recognition::invalid("x_meeting.device", "auto・cpu・gpuを指定してください。")
                })?;
            config.insert("device".into(), device.into());
        }
    }
    let modern = value.get("audio").and_then(|v| v.get("input"));
    if let Some(audio) = value.get("audio") {
        check_keys(audio, &["input"], "session.audio")?;
    }
    if let Some(input) = modern {
        check_keys(
            input,
            &["format", "transcription", "turn_detection"],
            "session.audio.input",
        )?;
        if let Some(format) = input.get("format")
            && *format != json!({"type":"audio/pcm","rate":24000})
        {
            return Err(recognition::unsupported("session.audio.input.format"));
        }
    }
    if value
        .get("input_audio_format")
        .is_some_and(|v| v != "pcm16")
    {
        return Err(recognition::unsupported("input_audio_format"));
    }
    if value.get("turn_detection").is_some_and(|v| !v.is_null())
        || modern
            .and_then(|v| v.get("turn_detection"))
            .is_some_and(|v| !v.is_null())
    {
        return Err(recognition::unsupported("turn_detection"));
    }
    let transcription = modern
        .and_then(|v| v.get("transcription"))
        .or_else(|| value.get("input_audio_transcription"));
    if let Some(transcription) = transcription {
        check_keys(
            transcription,
            &["model", "language", "prompt"],
            "transcription",
        )?;
        for key in ["model", "language", "prompt"] {
            if let Some(value) = transcription.get(key) {
                let text = value
                    .as_str()
                    .ok_or_else(|| recognition::invalid(key, "文字列を指定してください。"))?;
                if text.len() > 8192 {
                    return Err(recognition::invalid(key, "入力が長すぎます。"));
                }
                config.insert(key.into(), text.into());
            }
        }
    }
    Ok(())
}
fn query(config: &HashMap<String, String>, client: &str) -> Result<HashMap<String, String>> {
    let full = config.get("model").context(recognition::invalid(
        "model",
        "/v1/modelsのモデルIDを指定してください。",
    ))?;
    let (engine, model) = full
        .split_once('/')
        .ok_or_else(|| recognition::invalid("model", "エンジン/モデルのIDを指定してください。"))?;
    if matches!(
        engine,
        "diarization" | crate::integrated::ENGINE | "openai" | "compatible"
    ) {
        return Err(recognition::unsupported("model"));
    }
    let mut q = HashMap::from([
        ("engine".into(), engine.into()),
        ("model".into(), model.into()),
        (
            "device".into(),
            config
                .get("device")
                .cloned()
                .unwrap_or_else(|| "auto".into()),
        ),
        ("diarize".into(), "false".into()),
        ("clientId".into(), client.into()),
        ("workload".into(), "realtime".into()),
        ("name".into(), "realtime.wav".into()),
    ]);
    for key in ["language", "prompt"] {
        if let Some(value) = config.get(key) {
            q.insert(key.into(), value.clone());
        }
    }
    if !q.contains_key("language") && matches!(engine, "whisper" | "qwen" | crate::nemo::ENGINE) {
        q.insert("language".into(), "auto".into());
    }
    recognition::validate(engine, &q)?;
    Ok(q)
}
fn session_value(id: &str, config: &HashMap<String, String>) -> Value {
    let transcription: HashMap<_, _> = config
        .iter()
        .filter(|(key, _)| key.as_str() != "device")
        .collect();
    json!({"id":id,"object":"realtime.transcription_session","type":"transcription",
        "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":transcription,"turn_detection":null}},
        "x_meeting":{"device":config.get("device").map(String::as_str).unwrap_or("auto"),"maxCommitSeconds":30,"maxPendingCommits":4,"partialResults":"committed_segments_only"}})
}
fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(pcm.len() + 44);
    data.extend(b"RIFF");
    data.extend((pcm.len() as u32 + 36).to_le_bytes());
    data.extend(b"WAVEfmt ");
    data.extend(16u32.to_le_bytes());
    data.extend(1u16.to_le_bytes());
    data.extend(1u16.to_le_bytes());
    data.extend(24000u32.to_le_bytes());
    data.extend(48000u32.to_le_bytes());
    data.extend(2u16.to_le_bytes());
    data.extend(16u16.to_le_bytes());
    data.extend(b"data");
    data.extend((pcm.len() as u32).to_le_bytes());
    data.extend(pcm);
    data
}
async fn accept(
    app: &Arc<App>,
    config: &HashMap<String, String>,
    client: &str,
    pcm: &[u8],
) -> Result<String> {
    if pcm.len() < 4800 {
        return Err(recognition::invalid(
            "audio",
            "0.1秒以上の音声を送ってから確定してください。",
        ));
    }
    let q = query(config, client)?;
    if fs2::available_space(&app.store.dir)? < MAX_PCM as u64 + 128 * 1024 * 1024 {
        bail!("保存先の空き容量が不足しています。")
    }
    let job = app.create(&q).await?;
    let id = job["id"].as_str().unwrap().to_owned();
    let mut owned = OwnedJobs::new(app.clone());
    owned.ids.insert(id.clone());
    let data = wav(pcm);
    let mut file = tokio::fs::File::create(app.store.dir.join(&id).join("source.media")).await?;
    file.write_all(&data).await?;
    file.sync_data().await?;
    drop(file);
    {
        let _gate = app.admission.lock().await;
        app.store
            .register_media(&id, &hex::encode(Sha256::digest(&data)), data.len() as u64)?;
    }
    app.enqueue(&id).await?;
    owned.ids.remove(&id);
    Ok(id)
}
async fn watch(app: Arc<App>, id: String, item: String, tx: mpsc::Sender<(Option<String>, Value)>) {
    let mut owned = OwnedJobs::new(app.clone());
    owned.ids.insert(id.clone());
    let result=async {
        let mut cursor=Cursor::new(app,&id)?;
        loop {
            if tx.is_closed() { bail!("接続が閉じられました。") }
            let batch=cursor.poll()?;
            for delta in batch.deltas {
                send(&tx,(None,event(json!({"type":"conversation.item.input_audio_transcription.delta","item_id":item,"content_index":0,"delta":delta})))).await?;
            }
            if batch.done {
                return send(&tx,(Some(item.clone()),event(json!({"type":"conversation.item.input_audio_transcription.completed","item_id":item,"content_index":0,"transcript":cursor.text,"x_meeting":{"jobId":id,"resultGeneration":cursor.generation}})))).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }.await;
    if let Err(error) = result {
        let _=send(&tx,(Some(item.clone()),event(json!({"type":"conversation.item.input_audio_transcription.failed","item_id":item,"content_index":0,"error":{"type":"server_error","code":"transcription_failed","message":error.to_string()},"x_meeting":{"jobId":id}})))).await;
    }
}
async fn transmit(socket: &mut WebSocket, value: Value) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(10),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await??;
    Ok(())
}
async fn session(app: Arc<App>, mut socket: WebSocket, model: Option<String>) -> Result<()> {
    let id = format!("rt_{}", uuid::Uuid::new_v4().simple());
    let mut config = HashMap::new();
    if let Some(model) = model {
        config.insert("model".into(), model);
    }
    let mut owned = OwnedJobs::new(app.clone());
    let mut jobs = HashMap::<String, String>::new();
    let mut previous: Option<String> = None;
    let mut pcm = vec![];
    let (tx, mut rx) = mpsc::channel(16);
    transmit(
        &mut socket,
        event(json!({"type":"session.created","session":session_value(&id,&config)})),
    )
    .await?;
    loop {
        tokio::select! {
            _=app.stop.cancelled()=>break,
            notification=rx.recv()=>{
                if let Some((terminal,message))=notification {
                    if let Some(item)=terminal && let Some(job)=jobs.remove(&item) { owned.ids.remove(&job); }
                    transmit(&mut socket,message).await?;
                }
            }
            incoming=tokio::time::timeout(Duration::from_secs(120),socket.recv())=>{
                let Some(message)=incoming.context("リアルタイム接続がタイムアウトしました。")? else {break;};
                let message=message?;
                let Message::Text(raw)=message else {
                    if matches!(message,Message::Close(_)) {break;} else {continue;}
                };
                let value:Value=match serde_json::from_str(raw.as_str()) {
                    Ok(value)=>value,
                    Err(_)=>{transmit(&mut socket,error_event(&recognition::invalid("event","JSONを指定してください。"),&Value::Null)).await?;continue;}
                };
                let result:Result<()>=async {
                    match value["type"].as_str().unwrap_or("") {
                        "session.update"|"transcription_session.update"=>{
                            if !pcm.is_empty()||!jobs.is_empty() {return Err(recognition::invalid("session","音声バッファと処理中の発言が空のときに変更してください。"));}
                            let mut next=config.clone();update(&mut next,&value["session"])?;
                            let q=query(&next,&id)?;
                            app.selected(&q["engine"],&q["model"],&q["device"],false)?;
                            config=next;
                            let kind=if value["type"]=="transcription_session.update" {"transcription_session.updated"} else {"session.updated"};
                            transmit(&mut socket,event(json!({"type":kind,"session":session_value(&id,&config)}))).await?;
                        }
                        "input_audio_buffer.append"=>{
                            let raw=value["audio"].as_str().ok_or_else(||recognition::invalid("audio","base64音声を指定してください。"))?;
                            append(&mut pcm,raw)?;
                        }
                        "input_audio_buffer.clear"=>{pcm.clear();transmit(&mut socket,event(json!({"type":"input_audio_buffer.cleared"}))).await?;}
                        "input_audio_buffer.commit"=>{
                            if jobs.len()>=4 {return Err(recognition::invalid("commit","未完了の発言は4件までです。"));}
                            let job=accept(&app,&config,&id,&pcm).await?;pcm.clear();
                            let item=format!("item_{}",uuid::Uuid::new_v4().simple());
                            owned.ids.insert(job.clone());jobs.insert(item.clone(),job.clone());
                            transmit(&mut socket,event(json!({"type":"input_audio_buffer.committed","previous_item_id":previous,"item_id":item,"x_meeting":{"jobId":job}}))).await?;
                            previous=Some(item.clone());tokio::spawn(watch(app.clone(),job,item,tx.clone()));
                        }
                        "transcription.cancel"=>{
                            let item=value["item_id"].as_str().ok_or_else(||recognition::invalid("item_id","発言IDを指定してください。"))?;
                            let job=jobs.get(item).ok_or_else(||recognition::invalid("item_id","この接続で処理中の発言ではありません。"))?;
                            app.cancel(job)?;
                        }
                        _=>return Err(recognition::unsupported("type")),
                    }
                    Ok(())
                }.await;
                if let Err(error)=result {transmit(&mut socket,error_event(&error,&value)).await?;}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audio_limit_rejects_without_corrupting_previous_buffer() {
        let mut pcm = vec![0, 0];
        assert!(append(&mut pcm, "AA==").is_err());
        assert!(append(&mut pcm, "invalid").is_err());
        let large = base64::engine::general_purpose::STANDARD.encode(vec![0; MAX_PCM]);
        assert!(append(&mut pcm, &large).is_err());
        assert_eq!(pcm, vec![0, 0]);
    }
    #[tokio::test]
    async fn sse_is_once_only_stops_on_new_generation_and_cancels_on_disconnect() {
        let dir = std::env::temp_dir().join(format!("tc-stream-{}", uuid::Uuid::new_v4()));
        let app = App::new(crate::config::Runtime {
            config: crate::config::Config {
                host: "127.0.0.1".into(),
                lan: false,
                data_dir: dir.join("data"),
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
        let job=app.store.create(json!({"status":"running","phase":"transcribing","metadata":{"resultGeneration":0},"segmentCount":0})).unwrap();
        let id = job["id"].as_str().unwrap();
        app.store
            .append(id, json!({"start":0,"end":1,"text":"一"}))
            .unwrap();
        let mut cursor = Cursor::new(app.clone(), id).unwrap();
        assert_eq!(cursor.poll().unwrap().deltas, vec!["一"]);
        assert!(cursor.poll().unwrap().deltas.is_empty());
        app.store
            .update(id, json!({"metadata":{"resultGeneration":1}}))
            .unwrap();
        assert!(cursor.poll().is_err());
        app.store
            .update(id, json!({"status":"complete","phase":"complete"}))
            .unwrap();
        let response = sse(app.clone(), id).unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(text.matches("event: transcript.text.delta").count(), 1);
        assert_eq!(text.matches("event: transcript.text.done").count(), 1);
        let pending = app
            .store
            .create(json!({"status":"queued","metadata":{"resultGeneration":0}}))
            .unwrap();
        let pid = pending["id"].as_str().unwrap();
        drop(sse(app.clone(), pid).unwrap());
        for _ in 0..50 {
            if app.store.get(pid).unwrap()["status"] == "cancelled" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(app.store.get(pid).unwrap()["status"], "cancelled");
        app.stop.cancel();
    }
    #[test]
    fn manual_session_updates_are_atomic_bounded_and_reject_unsupported_audio() {
        let mut config = HashMap::new();
        update(&mut config,&json!({"type":"transcription","audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"qwen/qwen3-asr-0.6b","prompt":"TranscriptCutter","language":"en"},"turn_detection":null}}})).unwrap();
        let q = query(&config, "test").unwrap();
        assert_eq!(q["prompt"], "TranscriptCutter");
        assert_eq!(q["workload"], "realtime");
        assert!(
            update(
                &mut config,
                &json!({"turn_detection":{"type":"server_vad"}})
            )
            .is_err()
        );
        assert!(update(&mut config, &json!({"input_audio_format":"g711_ulaw"})).is_err());
        assert_eq!(&wav(&[0, 0])[0..4], b"RIFF");
        assert_eq!(wav(&[0, 0]).len(), 46);
    }
}
