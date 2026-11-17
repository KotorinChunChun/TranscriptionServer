use crate::{
    app::App,
    catalog,
    network::equal,
    store::{active, id_valid},
};
use anyhow::Context;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Multipart, State},
    http::{HeaderValue, Method, Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
const MAX_UPLOAD: u64 = 16 * 1024 * 1024 * 1024;
#[derive(Debug)]
pub struct ApiError(
    pub StatusCode,
    pub String,
    pub Option<crate::recognition::InputError>,
);
impl ApiError {
    pub fn new(status: StatusCode, message: String) -> Self {
        Self(status, message, None)
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let message = error.to_string();
        let status = if message == "NOT_FOUND" {
            StatusCode::NOT_FOUND
        } else if message.starts_with("UNAVAILABLE:") {
            StatusCode::SERVICE_UNAVAILABLE
        } else if message.starts_with("FORBIDDEN:") {
            StatusCode::FORBIDDEN
        } else if message.starts_with("DELETE_FAILED:") {
            StatusCode::INTERNAL_SERVER_ERROR
        } else if message.starts_with("CONFLICT:") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        Self(
            status,
            message,
            error
                .downcast_ref::<crate::recognition::InputError>()
                .cloned(),
        )
    }
}
impl From<std::io::Error> for ApiError {
    fn from(_: std::io::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "保存先または実行環境を確認してください。".into(),
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({"error":self.1}))).into_response()
    }
}
fn json_response(value: Value) -> Response {
    axum::Json(value).into_response()
}
fn err(status: StatusCode, message: &str) -> ApiError {
    ApiError::new(status, message.into())
}
fn query(request: &Request<Body>) -> HashMap<String, String> {
    url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}
fn header<'a>(request: &'a Request<Body>, name: &str) -> Option<&'a str> {
    request.headers().get(name).and_then(|v| v.to_str().ok())
}
fn authorize(
    app: &App,
    peer: SocketAddr,
    request: &Request<Body>,
) -> std::result::Result<(), ApiError> {
    let host = header(request, "host").unwrap_or("");
    if !app.policy.peer(peer.ip())
        || request.headers().get_all("host").iter().count() != 1
        || !app.policy.host(host)
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            "許可された接続元/Hostを確認してください。",
        ));
    }
    if !app.policy.origin(header(request, "origin")) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "このページからの接続は許可されていません。",
        ));
    }
    if (request.uri().path().starts_with("/api/updates")
        || request.uri().path().starts_with("/api/admin/"))
        && !peer.ip().is_loopback()
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            "更新管理はサーバーPC上で利用してください。",
        ));
    }
    if request.uri().path() == "/api/management/restart"
        && !peer.ip().is_loopback()
        && !app.runtime.config.lan
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            "再起動はサーバーPCまたは信頼LANから操作してください。",
        ));
    }
    if request.method() == Method::OPTIONS {
        return Ok(());
    }
    let path = request.uri().path();
    let websocket = path == "/v1/realtime";
    let mutation = !matches!(*request.method(), Method::GET | Method::HEAD) || websocket;
    let protocol_key = if websocket {
        let keys: Vec<_> = header(request, "sec-websocket-protocol")
            .unwrap_or("")
            .split(',')
            .filter_map(|v| v.trim().strip_prefix("openai-insecure-api-key."))
            .collect();
        if keys.len() == 1 { keys[0] } else { "" }
    } else {
        ""
    };
    if app.runtime.config.server
        && (path.starts_with("/api/") || path.starts_with("/v1/") || mutation)
    {
        let supplied = header(request, "authorization").unwrap_or("");
        let media = matches!(*request.method(), Method::GET | Method::HEAD)
            && valid_media(app, path, request.uri().query().unwrap_or(""));
        if !equal(supplied, &format!("Bearer {}", app.key))
            && !media
            && !(websocket && equal(protocol_key, &app.key))
        {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "サーバーの接続キーを確認してください。",
            ));
        }
    } else if mutation {
        let local = equal(header(request, "x-local-token").unwrap_or(""), &app.token);
        let bearer = path.starts_with("/v1/")
            && equal(
                header(request, "authorization").unwrap_or(""),
                &format!("Bearer {}", app.token),
            );
        if !local && !bearer && !(websocket && equal(protocol_key, &app.token)) {
            return Err(err(
                StatusCode::FORBIDDEN,
                "接続を更新してから操作してください。",
            ));
        }
    }
    Ok(())
}
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .fallback(dispatch)
        .layer(DefaultBodyLimit::disable())
        .with_state(app)
}
async fn dispatch(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response {
    let path = request.uri().path().to_string();
    let origin = header(&request, "origin").map(str::to_owned);
    let method = request.method().clone();
    let outcome = match authorize(&app, peer, &request) {
        Ok(()) => route(app.clone(), request).await,
        Err(e) => Err(e),
    };
    let mut response = match outcome {
        Ok(response) => response,
        Err(error) => {
            if path.starts_with("/v1/") {
                let (kind, code) = match error.0 {
                    StatusCode::UNAUTHORIZED => ("authentication_error", "invalid_api_key"),
                    StatusCode::FORBIDDEN => ("permission_error", "permission_denied"),
                    StatusCode::NOT_FOUND => ("invalid_request_error", "not_found"),
                    StatusCode::SERVICE_UNAVAILABLE => ("server_error", "service_unavailable"),
                    StatusCode::BAD_GATEWAY => ("server_error", "transcription_failed"),
                    status if status.is_server_error() => ("server_error", "internal_error"),
                    _ => ("invalid_request_error", "invalid_request"),
                };
                let body = json!({"error":{"message":error.1,"type":kind,
                    "param":error.2.as_ref().map(|v| &v.param),
                    "code":error.2.as_ref().map(|v|v.code).unwrap_or(code)}});
                (error.0, axum::Json(body)).into_response()
            } else {
                error.into_response()
            }
        }
    };
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if response.status() == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("5"));
    }
    if let Some(origin) = origin.filter(|v| app.policy.origin(Some(v)))
        && let Ok(value) = HeaderValue::from_str(&origin)
    {
        response
            .headers_mut()
            .insert("access-control-allow-origin", value);
        response
            .headers_mut()
            .insert("vary", HeaderValue::from_static("Origin"));
        response.headers_mut().insert(
            "access-control-expose-headers",
            HeaderValue::from_static("Content-Range, Accept-Ranges, X-Meeting-Job-Id, Retry-After"),
        );
    }
    if method == Method::HEAD {
        *response.body_mut() = Body::empty()
    }
    response
}
async fn body_json(request: Request<Body>) -> std::result::Result<Value, ApiError> {
    let raw = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| err(StatusCode::PAYLOAD_TOO_LARGE, "JSON本文は1MiB以下です。"))?;
    if raw.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&raw)
        .map_err(|_| err(StatusCode::BAD_REQUEST, "JSON本文を確認してください。"))
}
async fn route(app: Arc<App>, request: Request<Body>) -> std::result::Result<Response, ApiError> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let qs = query(&request);
    if method == Method::GET && path == "/v1/realtime" {
        return crate::streaming::upgrade(app, request).await;
    }
    if method == Method::POST && path == "/api/admin/drain" {
        let body = body_json(request).await?;
        let purpose = body["action"].as_str().unwrap_or("shutdown");
        if body["confirm"] != "drain-server" {
            return Err(err(StatusCode::BAD_REQUEST, "完了待ち停止の確認が必要です"));
        }
        return Ok(json_response(
            crate::lifecycle::request(app, purpose).await?,
        ));
    }
    if method == Method::POST && path == "/api/admin/drain/cancel" {
        let body = body_json(request).await?;
        return Ok(json_response(
            crate::lifecycle::cancel(&app, body["id"].as_str().unwrap_or("")).await?,
        ));
    }
    if method == Method::POST && path == "/api/management/restart" {
        let body = body_json(request).await?;
        if body["confirm"] != "restart-server" {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "サーバーの再起動を確認してください。",
            ));
        }
        crate::restart::request(&app).await?;
        return Ok(json_response(
            json!({"restarting":true,"previousProcessId":std::process::id()}),
        ));
    }
    if method == Method::POST && path == "/api/admin/shutdown" {
        body_json(request).await?;
        let _gate = app.admission.lock().await;
        if app.update_busy.load(std::sync::atomic::Ordering::Acquire)
            || (app.maintenance.load(std::sync::atomic::Ordering::Acquire) && !app.preparing())
            || app.incoming.load(std::sync::atomic::Ordering::Acquire) > 0
            || !app.store.active_jobs()?.is_empty()
        {
            return Err(err(
                StatusCode::CONFLICT,
                "処理/転送/更新中は停止できません。",
            ));
        }
        app.maintenance
            .store(true, std::sync::atomic::Ordering::Release);
        let stop = app.stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel();
        });
        return Ok(json_response(json!({"stopping":true})));
    }
    if method == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, HEAD, POST, PATCH, OPTIONS"),
        );
        response.headers_mut().insert(
            "access-control-allow-headers",
            HeaderValue::from_static("Content-Type, X-Local-Token, Authorization"),
        );
        response
            .headers_mut()
            .insert("access-control-max-age", HeaderValue::from_static("600"));
        if header(&request, "access-control-request-private-network") == Some("true") {
            response.headers_mut().insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
        }
        return Ok(response);
    }
    if matches!(method, Method::GET | Method::HEAD) {
        match path.as_str() {
            "/api/health" => return Ok(json_response(app.health())),
            "/v1/openapi.json" => {
                return Ok((
                    [("content-type", "application/json; charset=utf-8")],
                    include_str!("../docs/openapi.json"),
                )
                    .into_response());
            }
            "/v1/models" => {
                let mut value = catalog::catalog(&app.engines.lock().unwrap());
                crate::model_info::enrich(&app, &mut value);
                return Ok(json_response(value));
            }
            path if path.starts_with("/v1/models/") => {
                let id = percent_decode(&path["/v1/models/".len()..])?;
                let mut value = catalog::catalog(&app.engines.lock().unwrap());
                crate::model_info::enrich(&app, &mut value);
                let model = value["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["id"] == id)
                    .ok_or_else(|| {
                        err(StatusCode::NOT_FOUND, "選択したモデルが見つかりません。")
                    })?;
                return Ok(json_response(model.clone()));
            }
            "/api/models" => return Ok(json_response(crate::model_info::inventory(&app))),
            "/api/management" => return Ok(json_response(crate::models::status(&app).await?)),
            "/api/status" => return Ok(json_response(app.status().await?)),
            "/api/scheduling" => return Ok(json_response(crate::scheduler::status(&app).await)),
            "/api/storage" => {
                let dir = &app.store.dir;
                return Ok(json_response(
                    json!({"usedBytes":crate::store::tree_size(dir)?,
                    "freeBytes":fs2::available_space(dir)?,"totalBytes":fs2::total_space(dir)?,
                    "jobCount":app.store.page(1,0,None,None)?["total"]}),
                ));
            }
            "/api/updates" => return Ok(json_response(crate::updates::status(&app)?)),
            "/api/jobs" => {
                let value = if ["limit", "offset", "clientId", "workload"]
                    .iter()
                    .any(|k| qs.contains_key(*k))
                {
                    let limit = number(&qs, "limit", 50)?;
                    let offset = number(&qs, "offset", 0)?;
                    if !(1..=200).contains(&limit) {
                        return Err(err(StatusCode::BAD_REQUEST, "一覧件数は1～200です。"));
                    }
                    app.store.page(
                        limit,
                        offset,
                        qs.get("clientId").map(String::as_str),
                        qs.get("workload").map(String::as_str),
                    )?
                } else {
                    json!({"jobs":app.store.list(None,None)?})
                };
                return Ok(json_response(value));
            }
            _ => (),
        }
        if let Some((id, action)) = job_path(&path) {
            let job = app.store.get(id)?;
            return match action {
                "" => Ok(json_response(json!({"job":job}))),
                "segments" => Ok(json_response(app.store.segments(
                    id,
                    number(&qs, "after", 0)? as u64,
                    number(&qs, "limit", 1000)?.clamp(1, 5000),
                )?)),
                "media" => {
                    send_file(
                        &app.store.dir.join(id).join("source.media"),
                        header(&request, "range").map(str::to_owned),
                        Some(mime(job["fileName"].as_str().unwrap_or(""))),
                    )
                    .await
                }
                "media-link" => {
                    let (path, expires) = if app.runtime.config.server {
                        let expires = chrono::Utc::now().timestamp() + 43200;
                        (
                            format!(
                                "/api/jobs/{id}/media?expires={expires}&signature={}",
                                signature(&app, id, expires)
                            ),
                            Some(
                                chrono::DateTime::from_timestamp(expires, 0)
                                    .unwrap()
                                    .to_rfc3339(),
                            ),
                        )
                    } else {
                        (format!("/api/jobs/{id}/media"), None)
                    };
                    Ok(json_response(json!({"path":path,"expiresAt":expires})))
                }
                _ => Err(err(StatusCode::NOT_FOUND, "APIが見つかりません。")),
            };
        }
        if path.starts_with("/api/") || path.starts_with("/v1/") {
            return Err(err(StatusCode::NOT_FOUND, "APIが見つかりません。"));
        }
        let relative = url::Url::parse(&format!("http://localhost{path}"))
            .map_err(|_| err(StatusCode::BAD_REQUEST, "URLを確認してください。"))?;
        let decoded = percent_decode(relative.path())?;
        let relative = Path::new(decoded.trim_start_matches('/'));
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || decoded.contains('\\')
            || decoded.contains(':')
        {
            return Err(err(StatusCode::NOT_FOUND, "ファイルがありません。"));
        }
        let root = app.runtime.root.join("dist");
        let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dist");
        let root = if cfg!(debug_assertions) && !app.runtime.installed && !root.is_dir() {
            source_root
        } else {
            root
        };
        let asset = if matches!(decoded.as_str(), "/" | "/index.html") {
            "management.html"
        } else {
            decoded.trim_start_matches('/')
        };
        let assets: Vec<String> =
            serde_json::from_str(include_str!("../manifests/management-assets.json"))
                .context("管理画面の配信定義が不正です")?;
        if !assets.iter().any(|name| name == asset) {
            return Err(err(StatusCode::NOT_FOUND, "ファイルがありません。"));
        }
        let file = root.join(asset);
        let canonical = tokio::fs::canonicalize(&file)
            .await
            .map_err(|_| err(StatusCode::NOT_FOUND, "ファイルがありません。"))?;
        let root = tokio::fs::canonicalize(root).await?;
        if !canonical.starts_with(root) {
            return Err(err(StatusCode::NOT_FOUND, "ファイルがありません。"));
        }
        return send_file(
            &canonical,
            header(&request, "range").map(str::to_owned),
            None,
        )
        .await;
    }
    if method == Method::POST && path == "/api/management" {
        let body = body_json(request).await?;
        return Ok(json_response(crate::models::action(app, body).await?));
    }
    if method == Method::POST && path == "/api/management/scheduling" {
        let value = body_json(request).await?;
        return Ok(json_response(
            crate::scheduler::configure(&app, value).await?,
        ));
    }
    if method == Method::POST && path == "/api/jobs" {
        return raw_upload(app, request, qs).await;
    }
    if method == Method::POST && path == "/v1/audio/transcriptions" {
        return multipart_upload(app, request).await;
    }
    if matches!(path.as_str(), "/api/updates/check" | "/api/updates/apply")
        && method == Method::POST
    {
        let body = body_json(request).await?;
        return Ok(json_response(
            crate::updates::action(&app, &path, body)
                .await
                .map_err(|e| ApiError::new(StatusCode::CONFLICT, e.to_string()))?,
        ));
    }
    if let Some((id, action)) = job_path(&path) {
        if method == Method::POST {
            let body = body_json(request).await?;
            return match action {
                "delete" => Ok(json_response(
                    crate::deletion::request(&app, id, body).await?,
                )),
                "cancel" => Ok(json_response(json!({"job":app.cancel(id)?}))),
                "remove-media" => {
                    let _gate = app.admission.lock().await;
                    if app.maintenance.load(std::sync::atomic::Ordering::Acquire)
                        || app.draining.load(std::sync::atomic::Ordering::Acquire)
                        || app.update_busy.load(std::sync::atomic::Ordering::Acquire)
                        || app.incoming.load(std::sync::atomic::Ordering::Acquire) > 0
                        || !app.store.active_jobs()?.is_empty()
                    {
                        return Err(err(
                            StatusCode::CONFLICT,
                            "処理・転送・更新・モデル操作中は媒体を整理できません。",
                        ));
                    }
                    if body["confirm"] != "remove-media" {
                        return Err(err(
                            StatusCode::BAD_REQUEST,
                            "整理の確認値を指定してください。",
                        ));
                    }
                    Ok(json_response(json!({"job":app.store.remove_media(id)?})))
                }
                _ => Err(err(StatusCode::NOT_FOUND, "APIがありません。")),
            };
        }
        if method == Method::PATCH && (action == "segments" || action.starts_with("segments/")) {
            let mut body = body_json(request).await?;
            let single = action.strip_prefix("segments/");
            let edits = if let Some(sid) = single {
                body["id"] = sid
                    .parse::<u64>()
                    .map_err(|_| err(StatusCode::BAD_REQUEST, "字幕IDが不正です。"))?
                    .into();
                vec![body]
            } else {
                body["edits"]
                    .as_array()
                    .context("字幕編集を指定してください")?
                    .clone()
            };
            let rows = app.store.edit(id, &edits)?;
            return Ok(json_response(if single.is_some() {
                json!({"segment":rows[0]})
            } else {
                json!({"segments":rows,"job":app.store.get(id)?})
            }));
        }
    }
    Err(err(StatusCode::NOT_FOUND, "APIがありません。"))
}
fn number(
    qs: &HashMap<String, String>,
    key: &str,
    default: usize,
) -> std::result::Result<usize, ApiError> {
    qs.get(key)
        .map(|s| {
            s.parse()
                .map_err(|_| err(StatusCode::BAD_REQUEST, "数値を確認してください。"))
        })
        .unwrap_or(Ok(default))
}
fn job_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/jobs/")?;
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    id_valid(id).then_some((id, action))
}
fn mime(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "m4a" => "audio/mp4",
        "vtt" => "text/vtt; charset=utf-8",
        _ => "application/octet-stream",
    }
}
async fn send_file(
    path: &Path,
    requested_range: Option<String>,
    content: Option<&str>,
) -> std::result::Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "ファイルがありません。"))?;
    let size = file.metadata().await?.len();
    let (mut start, mut end, mut code) = (0, size.saturating_sub(1), StatusCode::OK);
    if let Some(range) = requested_range.as_deref() {
        let parsed = (|| {
            let raw = range.strip_prefix("bytes=")?;
            let (left, right) = raw.split_once('-')?;
            if left.is_empty() {
                let length = right.parse::<u64>().ok()?;
                if length == 0 {
                    return None;
                }
                Some((size.saturating_sub(length), size.saturating_sub(1)))
            } else {
                let start = left.parse::<u64>().ok()?;
                let end = if right.is_empty() {
                    size.saturating_sub(1)
                } else {
                    right.parse::<u64>().ok()?.min(size.saturating_sub(1))
                };
                Some((start, end))
            }
        })();
        if let Some((left, right)) =
            parsed.filter(|(left, right)| size > 0 && left <= right && *left < size)
        {
            start = left;
            end = right;
            code = StatusCode::PARTIAL_CONTENT
        } else {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response
                .headers_mut()
                .insert("content-range", format!("bytes */{size}").parse().unwrap());
            return Ok(response);
        }
    }
    let length = if size == 0 { 0 } else { end - start + 1 };
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file.take(length)));
    let mut response = (code, body).into_response();
    response
        .headers_mut()
        .insert("content-length", length.to_string().parse().unwrap());
    response.headers_mut().insert(
        "content-type",
        content
            .unwrap_or_else(|| mime(&path.to_string_lossy()))
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert("accept-ranges", HeaderValue::from_static("bytes"));
    if code == StatusCode::PARTIAL_CONTENT {
        response.headers_mut().insert(
            "content-range",
            format!("bytes {start}-{end}/{size}").parse().unwrap(),
        );
    }
    Ok(response)
}
struct UploadGuard {
    app: Arc<App>,
    id: String,
    accepted: bool,
    media_ready: bool,
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        if !self.accepted {
            let _ = self.app.store.update(
                &self.id,
                json!({"status":"error","phase":"error","error":"音声の転送が完了しませんでした。",
                    "message":"転送状態を確認してください。"}),
            );
            if !self.media_ready {
                let _ =
                    std::fs::remove_file(self.app.store.dir.join(&self.id).join("source.media"));
            }
        }
    }
}
struct Incoming {
    path: PathBuf,
    app: Arc<App>,
}
impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.app
            .incoming
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
fn upload_length(request: &Request<Body>, max: u64) -> std::result::Result<u64, ApiError> {
    if request.headers().contains_key("transfer-encoding")
        || request.headers().get_all("content-length").iter().count() != 1
    {
        return Err(err(
            StatusCode::LENGTH_REQUIRED,
            "Content-Lengthを指定してください。",
        ));
    }
    let length = header(request, "content-length")
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "Content-Lengthが不正です。"))?;
    if length > max {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ファイルの容量上限を超えています。",
        ));
    }
    if length == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "音声が空です。"));
    }
    Ok(length)
}
async fn raw_upload(
    app: Arc<App>,
    request: Request<Body>,
    qs: HashMap<String, String>,
) -> std::result::Result<Response, ApiError> {
    let length = upload_length(&request, MAX_UPLOAD)?;
    if fs2::available_space(&app.store.dir)? < length + 128 * 1024 * 1024 {
        return Err(err(
            StatusCode::INSUFFICIENT_STORAGE,
            "保存先の空き容量が不足しています。",
        ));
    }
    let job = app.create(&qs).await?;
    let id = job["id"].as_str().unwrap().to_string();
    let mut guard = UploadGuard {
        app: app.clone(),
        id: id.clone(),
        accepted: false,
        media_ready: false,
    };
    let mut file = tokio::fs::File::create(app.store.dir.join(&id).join("source.media")).await?;
    let mut count = 0u64;
    let mut digest = Sha256::new();
    let mut stream = request.into_body().into_data_stream();
    while let Some(data) = tokio::time::timeout(Duration::from_secs(120), stream.next())
        .await
        .map_err(|_| err(StatusCode::REQUEST_TIMEOUT, "転送がタイムアウトしました。"))?
    {
        let data = data.map_err(|_| err(StatusCode::BAD_REQUEST, "音声の転送が中断しました。"))?;
        count += data.len() as u64;
        if count > length || count > MAX_UPLOAD {
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "音声の長さが指定値と一致しません。",
            ));
        }
        file.write_all(&data).await?;
        digest.update(&data);
    }
    if count != length {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "音声の転送が完了していません。",
        ));
    }
    file.sync_data().await?;
    drop(file);
    {
        let _gate = app.admission.lock().await;
        app.store
            .register_media(&id, &hex::encode(digest.finalize()), count)?;
    }
    guard.media_ready = true;
    let job = app.enqueue(&id).await?;
    guard.accepted = true;
    Ok((StatusCode::CREATED, axum::Json(json!({"job":job}))).into_response())
}
async fn multipart_upload(
    app: Arc<App>,
    request: Request<Body>,
) -> std::result::Result<Response, ApiError> {
    let length = upload_length(&request, MAX_UPLOAD + 16 * 1024 * 1024)?;
    if fs2::available_space(&app.store.dir)? < length + 128 * 1024 * 1024 {
        return Err(err(
            StatusCode::INSUFFICIENT_STORAGE,
            "空き容量が不足しています。",
        ));
    }
    let incoming = {
        let _gate = app.admission.lock().await;
        if app.maintenance.load(std::sync::atomic::Ordering::Acquire)
            || app.draining.load(std::sync::atomic::Ordering::Acquire)
            || app.incoming.load(std::sync::atomic::Ordering::Acquire) >= 64
        {
            return Err(err(
                StatusCode::SERVICE_UNAVAILABLE,
                "更新中または転送数の上限です。",
            ));
        }
        app.incoming
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Incoming {
            path: app
                .store
                .dir
                .join(format!("incoming-{}.media", uuid::Uuid::new_v4().simple())),
            app: app.clone(),
        }
    };
    let mut form = Multipart::from_request(request, &())
        .await
        .map_err(|_| err(StatusCode::BAD_REQUEST, "multipartを指定してください。"))?;
    let mut fields = HashMap::new();
    let mut keywords = Vec::new();
    let mut name = None;
    let mut total = 0u64;
    let mut digest = Sha256::new();
    while let Some(mut field) = form
        .next_field()
        .await
        .map_err(|_| err(StatusCode::BAD_REQUEST, "multipart本文が不正です。"))?
    {
        let key = field.name().unwrap_or("").to_string();
        if key == "file" {
            if name.is_some() {
                return Err(err(StatusCode::BAD_REQUEST, "fileは1つ指定してください。"));
            }
            name = Some(field.file_name().unwrap_or("recording").to_string());
            let mut file = tokio::fs::File::create(&incoming.path).await?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| err(StatusCode::BAD_REQUEST, "音声転送に失敗しました。"))?
            {
                total += chunk.len() as u64;
                if total > MAX_UPLOAD {
                    return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "音声が大きすぎます。"));
                }
                file.write_all(&chunk).await?;
                digest.update(&chunk);
            }
            file.sync_data().await?;
        } else {
            if ![
                "model",
                "language",
                "response_format",
                "temperature",
                "prompt",
                "stream",
                "timestamp_granularities[]",
                "chunking_strategy",
                "diarize",
                "device",
                "keywords[]",
                "decoding",
            ]
            .contains(&key.as_str())
                || (fields.contains_key(&key) && key != "keywords[]")
                || field.file_name().is_some()
            {
                return Err(crate::recognition::invalid(
                    &key,
                    "未対応または重複した入力項目です。",
                )
                .into());
            }
            let mut raw = vec![];
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| err(StatusCode::BAD_REQUEST, "入力項目が不正です。"))?
            {
                if raw.len() + chunk.len() > 8192 {
                    return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "入力項目が長すぎます。"));
                }
                raw.extend_from_slice(&chunk)
            }
            let value = String::from_utf8(raw)
                .map_err(|_| err(StatusCode::BAD_REQUEST, "入力項目はUTF-8です。"))?;
            if key == "keywords[]" {
                keywords.push(value);
                if keywords.len() > 64 || keywords.iter().map(String::len).sum::<usize>() > 8192 {
                    return Err(crate::recognition::invalid(
                        "keywords",
                        "語句は64件・合計8192バイト以下です。",
                    )
                    .into());
                }
            } else {
                fields.insert(key, value);
            }
        }
    }
    let name = name.ok_or_else(|| err(StatusCode::BAD_REQUEST, "fileを指定してください。"))?;
    if total == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "音声が空です。"));
    }
    let get = |key: &str, default: &str| fields.get(key).cloned().unwrap_or_else(|| default.into());
    let full = get("model", "");
    let (engine, model) = full
        .split_once('/')
        .filter(|(e, m)| !e.is_empty() && !m.is_empty())
        .ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "/v1/modelsが返したモデルIDを指定してください。",
            )
        })?;
    if engine == "diarization" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "話者分離専用モデルはPOST /api/jobsから実行してください。",
        ));
    }
    let format = get("response_format", "json");
    if !matches!(
        format.as_str(),
        "json" | "verbose_json" | "diarized_json" | "text" | "srt" | "vtt"
    ) {
        return Err(crate::recognition::invalid(
            "response_format",
            "対応する結果形式を指定してください。",
        )
        .into());
    }
    let stream = match get("stream", "false").as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(crate::recognition::invalid(
                "stream",
                "trueまたはfalseを指定してください。",
            )
            .into());
        }
    };
    if stream && format != "json" {
        return Err(crate::recognition::unsupported("response_format").into());
    }
    if get("chunking_strategy", "auto") != "auto" {
        return Err(crate::recognition::unsupported("chunking_strategy").into());
    }
    if fields
        .get("timestamp_granularities[]")
        .is_some_and(|v| v != "segment" || format != "verbose_json")
    {
        return Err(crate::recognition::unsupported("timestamp_granularities[]").into());
    }
    let automatic_language = crate::recognition::runtime_capabilities(
        engine,
        &app.engines.lock().unwrap(),
    )["automaticLanguage"]
        == true;
    let mut qs = HashMap::from([
        ("engine".into(), engine.into()),
        ("model".into(), model.into()),
        ("name".into(), name),
        (
            "diarize".into(),
            if format == "diarized_json" {
                "true".into()
            } else {
                get("diarize", "false")
            },
        ),
        ("device".into(), get("device", "auto")),
        (
            "language".into(),
            get("language", if automatic_language { "auto" } else { "ja" }),
        ),
        ("clientId".into(), "openai-api".into()),
    ]);
    for key in ["prompt", "decoding", "temperature"] {
        if let Some(value) = fields.get(key) {
            qs.insert(key.into(), value.clone());
        }
    }
    if matches!(engine, "openai" | "compatible") && !fields.contains_key("language") {
        qs.remove("language");
    }
    if !keywords.is_empty() {
        qs.insert("keywords".into(), serde_json::to_string(&keywords).unwrap());
    }
    let job = app.create_admitted(&qs, true).await?;
    let id = job["id"].as_str().unwrap().to_string();
    let mut guard = UploadGuard {
        app: app.clone(),
        id: id.clone(),
        accepted: false,
        media_ready: false,
    };
    tokio::fs::rename(&incoming.path, app.store.dir.join(&id).join("source.media")).await?;
    {
        let _gate = app.admission.lock().await;
        app.store
            .register_media(&id, &hex::encode(digest.finalize()), total)?;
    }
    guard.media_ready = true;
    app.enqueue(&id).await?;
    guard.accepted = true;
    if stream {
        return Ok(crate::streaming::sse(app, &id)?);
    }
    loop {
        let job = app.store.get(&id)?;
        if !active(&job) {
            if job["status"] != "complete" {
                return Err(err(
                    StatusCode::BAD_GATEWAY,
                    job["error"]
                        .as_str()
                        .unwrap_or("音声処理が完了しませんでした。"),
                ));
            }
            let mut segments = vec![];
            let mut after = 0;
            loop {
                let page = app.store.segments(&id, after, 5000)?;
                let rows = page["segments"].as_array().unwrap();
                if rows.is_empty() {
                    break;
                }
                after = page["next"].as_u64().unwrap();
                segments.extend(rows.iter().cloned())
            }
            let mut response = render(&job, &segments, &format);
            response
                .headers_mut()
                .insert("x-meeting-job-id", id.parse().unwrap());
            return Ok(response);
        }
        tokio::select! {
            _ = app.stop.cancelled() => {
                return Err(err(StatusCode::SERVICE_UNAVAILABLE, "サーバーを停止しています。"))
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => (),
        }
    }
}
fn timestamp(value: f64, comma: bool) -> String {
    let n = (value.max(0.) * 1000.).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        n / 3600000,
        (n / 60000) % 60,
        (n / 1000) % 60,
        if comma { ',' } else { '.' },
        n % 1000
    )
}
fn render(job: &Value, segments: &[Value], format: &str) -> Response {
    let text = segments
        .iter()
        .filter_map(|s| s["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if format == "text" {
        return (
            [("content-type", "text/plain; charset=utf-8")],
            format!("{text}\n"),
        )
            .into_response();
    }
    if matches!(format, "srt" | "vtt") {
        let mut text = if format == "vtt" {
            "WEBVTT\n\n".to_string()
        } else {
            String::new()
        };
        for (i, s) in segments.iter().enumerate() {
            text.push_str(&format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                timestamp(s["start"].as_f64().unwrap(), format == "srt"),
                timestamp(s["end"].as_f64().unwrap(), format == "srt"),
                s["text"].as_str().unwrap_or("")
            ))
        }
        return (
            [(
                "content-type",
                if format == "vtt" {
                    "text/vtt; charset=utf-8"
                } else {
                    "application/x-subrip; charset=utf-8"
                },
            )],
            text,
        )
            .into_response();
    }
    let rows = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut row = json!({"id":i,"start":s["start"],"end":s["end"],"text":s["text"]});
            if format == "verbose_json" {
                for key in [
                    "avg_logprob",
                    "compression_ratio",
                    "no_speech_prob",
                    "seek",
                    "temperature",
                    "tokens",
                ] {
                    if let Some(value) = s.get(key) {
                        row[key] = value.clone();
                    }
                }
            }
            if format == "diarized_json" {
                row["id"] = i.to_string().into();
                row["type"] = "transcript.text.segment".into();
                row["speaker"] = s["speaker"].clone()
            }
            row
        })
        .collect::<Vec<_>>();
    let mut payload = json!({"text":text,"x_meeting":{"jobId":job["id"],"timestampType":job["metadata"]["timestampType"],"processingMethod":job["device"]}});
    if format == "verbose_json" {
        payload["task"] = "transcribe".into();
        if let Some(language) = crate::recognition::response_language(job) {
            payload["language"] = language.into();
        }
        payload["duration"] = (job["rangeEnd"].as_f64().unwrap_or(0.)
            - job["rangeStart"].as_f64().unwrap_or(0.))
        .into();
        payload["segments"] = json!(rows)
    } else if format == "diarized_json" {
        payload["segments"] = json!(rows);
        payload["task"] = "transcribe".into();
        payload["duration"] = (job["rangeEnd"].as_f64().unwrap_or(0.)
            - job["rangeStart"].as_f64().unwrap_or(0.))
        .into();
    }
    json_response(payload)
}
fn signature(app: &App, id: &str, expires: i64) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(app.key.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("haas-meeting-media:v1\n{id}\n{expires}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
fn valid_media(app: &App, path: &str, query: &str) -> bool {
    let Some((id, "media")) = job_path(path) else {
        return false;
    };
    let pairs = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<Vec<_>>();
    if pairs.len() != 2 {
        return false;
    }
    let params = pairs.into_iter().collect::<HashMap<_, _>>();
    let Some(expires) = params
        .get("expires")
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| {
            *n > chrono::Utc::now().timestamp() && *n <= chrono::Utc::now().timestamp() + 43200
        })
    else {
        return false;
    };
    params
        .get("signature")
        .is_some_and(|s| equal(s, &signature(app, id, expires)))
}
fn percent_decode(value: &str) -> std::result::Result<String, ApiError> {
    let mut bytes = vec![];
    let input = value.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            if i + 2 >= input.len() {
                return Err(err(StatusCode::BAD_REQUEST, "URLが不正です。"));
            }
            let hex = std::str::from_utf8(&input[i + 1..i + 3])
                .map_err(|_| err(StatusCode::BAD_REQUEST, "URLが不正です。"))?;
            bytes.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| err(StatusCode::BAD_REQUEST, "URLが不正です。"))?,
            );
            i += 3
        } else {
            bytes.push(input[i]);
            i += 1
        }
    }
    String::from_utf8(bytes).map_err(|_| err(StatusCode::BAD_REQUEST, "URLはUTF-8です。"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn verbose_response_preserves_measured_whisper_statistics_without_fabricating_them() {
        let job = json!({"id":"job","device":"cpu","rangeStart":0,"rangeEnd":1,"metadata":{"language":"en"}});
        let segment = json!({"id":1,"start":0.1,"end":0.5,"text":"test","avg_logprob":-0.3,
            "compression_ratio":1.2,"no_speech_prob":0.01,"seek":42,"temperature":0.2,"tokens":[100,200]});
        let response = render(&job, std::slice::from_ref(&segment), "verbose_json");
        let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let output: Value = serde_json::from_slice(&body).unwrap();
        for key in [
            "avg_logprob",
            "compression_ratio",
            "no_speech_prob",
            "seek",
            "temperature",
            "tokens",
        ] {
            assert_eq!(output["segments"][0][key], segment[key]);
        }
        let native = json!({"start":0.1,"end":0.5,"text":"native"});
        let body = to_bytes(
            render(&job, &[native], "verbose_json").into_body(),
            1_000_000,
        )
        .await
        .unwrap();
        let output: Value = serde_json::from_slice(&body).unwrap();
        assert!(output["segments"][0].get("avg_logprob").is_none());
    }
    #[tokio::test]
    async fn model_capabilities_and_parameter_errors_are_http_visible_without_jobs() {
        let dir = std::env::temp_dir().join(format!("tc-options-{}", uuid::Uuid::new_v4()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = App::new(crate::config::Runtime {
            config: crate::config::Config {
                host: "127.0.0.1".into(),
                port,
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
        *app.engines.lock().unwrap() = json!([{"id":"haas","ready":true,"models":[{"id":"haas-ja-v1.0","label":"Haas","diarization":true,"availability":"installed"}],"processingMethods":[{"id":"auto"},{"id":"cpu"}]}]);
        let target = app.clone();
        let stop = app.stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router(target).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
        });
        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let specification: Value = client
            .get(format!("{url}/v1/openapi.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(specification["openapi"], "3.1.0");
        assert_eq!(specification["info"]["version"], crate::config::VERSION);
        assert!(specification["paths"]["/v1/realtime"]["get"]["x-websocket"].is_object());
        let model: Value = client
            .get(format!("{url}/v1/models/haas%2Fhaas-ja-v1.0"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(model["x_meeting"]["capabilities"]["keywords"], true);
        assert_eq!(
            client
                .get(format!("{url}/v1/models/not-found"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        for (key, value, param) in [
            ("prompt", "context", "prompt"),
            ("language", "en", "language"),
            ("stream", "invalid", "stream"),
            (
                "timestamp_granularities[]",
                "word",
                "timestamp_granularities[]",
            ),
        ] {
            let form = reqwest::multipart::Form::new()
                .text("model", "haas/haas-ja-v1.0")
                .text(key, value)
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(b"test".to_vec()).file_name("test.wav"),
                );
            let response = client
                .post(format!("{url}/v1/audio/transcriptions"))
                .header("X-Local-Token", &app.token)
                .multipart(form)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error: Value = response.json().await.unwrap();
            assert_eq!(error["error"]["param"], param);
            assert_eq!(app.store.page(1, 0, None, None).unwrap()["total"], 0);
        }
        app.stop.cancel();
        server.await.unwrap();
    }
    #[test]
    fn subtitle_formats_preserve_time() {
        let job = json!({"id":"x","device":"cpu","rangeStart":0,"rangeEnd":5,"metadata":{}});
        let segments = vec![json!({"start":0.5,"end":2,"text":"日本語","speaker":"話者1"})];
        assert_eq!(timestamp(0.5, true), "00:00:00,500");
        assert_eq!(
            render(&job, &segments, "verbose_json").status(),
            StatusCode::OK
        );
        assert!(job_path("/api/jobs/../../secrets").is_none());
        assert!(percent_decode("/%xx").is_err());
    }

    #[tokio::test]
    async fn actual_http_auth_atomic_edits_range_and_reopen() {
        let dir = std::env::temp_dir().join(format!("tc-http-{}", uuid::Uuid::new_v4()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = crate::config::Config {
            host: "127.0.0.1".into(),
            port,
            lan: false,
            data_dir: dir.join("data"),
            whisper_worker: Some(dir.join("missing-worker.exe")),
            ..Default::default()
        };
        let runtime = crate::config::Runtime {
            config,
            root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            home: dir.join("profile"),
            installed: false,
            process_record: None,
            open: false,
        };
        let app = App::new(runtime).await.unwrap();
        let job=app.store.create(json!({"status":"running","phase":"transcribing","segmentCount":0,"processedUntil":0,"metadata":{"legacyExtra":"keep"},"fileName":"test.wav"})).unwrap();
        let id = job["id"].as_str().unwrap();
        app.store
            .append(
                id,
                json!({"start":0.5,"end":2,"text":"元字幕","speaker":"話者1"}),
            )
            .unwrap();
        app.store
            .update(id, json!({"status":"complete","phase":"complete"}))
            .unwrap();
        std::fs::write(app.store.dir.join(id).join("source.media"), b"0123456789").unwrap();
        let target = app.clone();
        let stop = app.stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router(target).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
        });
        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .post(format!("{url}/api/jobs/{id}/cancel"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .get(format!("{url}/api/health"))
                .header("Origin", "https://untrusted.test")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .get(format!("{url}/api/health"))
                .header("Host", format!("untrusted.test:{port}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let health: Value = client
            .get(format!("{url}/api/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["runtime"], "rust");
        assert_eq!(
            client
                .post(format!("{url}/api/management"))
                .json(&json!({"action":"refresh"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .post(format!("{url}/api/management"))
                .header("X-Local-Token", &app.token)
                .json(&json!({"action":"install","model":"whisper/../../escape"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let pending = app
            .store
            .create(json!({"status":"queued","segmentCount":0,"metadata":{}}))
            .unwrap();
        for body in [
            json!({"action":"install-runtime","runtime":"speech"}),
            json!({"action":"install-runtime","runtime":"ffmpeg"}),
        ] {
            assert_eq!(
                client
                    .post(format!("{url}/api/management"))
                    .header("X-Local-Token", &app.token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            client
                .post(format!("{url}/api/management"))
                .header("X-Local-Token", &app.token)
                .json(&json!({"action":"load","model":"whisper/tiny"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        app.store
            .update(
                pending["id"].as_str().unwrap(),
                json!({"status":"cancelled"}),
            )
            .unwrap();
        let response = client
            .get(format!("{url}/api/jobs/{id}/media"))
            .header("Range", "bytes=2-5")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap(), &b"2345"[..]);
        let before = app.store.segments(id, 0, 10).unwrap();
        let response = client
            .patch(format!("{url}/api/jobs/{id}/segments"))
            .header("X-Local-Token", &app.token)
            .json(&json!({"edits":[{"id":1,"text":"must rollback"},{"id":999,"text":"absent"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(before, app.store.segments(id, 0, 10).unwrap());
        let response = client
            .patch(format!("{url}/api/jobs/{id}/segments"))
            .header("X-Local-Token", &app.token)
            .json(&json!({"edits":[{"id":1,"text":"編集後","speaker":"手動話者"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let edited = app.store.segments(id, 0, 10).unwrap();
        assert_eq!(edited["segments"][0]["start"], 0.5);
        assert_eq!(edited["segments"][0]["speakerSource"], "manual");
        assert_eq!(
            client
                .get(format!("{url}/api/jobs?limit=0"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let preserved = app.store.get(id).unwrap();
        let form = reqwest::multipart::Form::new()
            .text("model", "unknown/model")
            .part(
                "file",
                reqwest::multipart::Part::bytes(b"synthetic".to_vec()).file_name("test.wav"),
            );
        assert_eq!(
            client
                .post(format!("{url}/v1/audio/transcriptions"))
                .header("X-Local-Token", &app.token)
                .multipart(form)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(app.incoming.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(!std::fs::read_dir(&app.store.dir).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("incoming-")
        }));
        let id = id.to_string();
        app.stop.cancel();
        server.await.unwrap();
        for _ in 0..100 {
            if Arc::strong_count(&app) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(app);
        let reopened = crate::store::Store::open(&dir.join("data")).unwrap();
        assert_eq!(reopened.get(&id).unwrap(), preserved);
        assert_eq!(reopened.segments(&id, 0, 10).unwrap(), edited);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
