use crate::{app::App, http::ApiError};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};

pub async fn request(app: &Arc<App>, id: &str, body: Value) -> Result<Value, ApiError> {
    if body["confirm"] != "delete-job" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "削除の確認値を指定してください。".into(),
        ));
    }
    let client_id = body["clientId"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "利用アプリを指定してください。".into(),
            )
        })?
        .to_owned();
    let _gate = app.admission.lock().await;
    if app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.update_busy.load(Ordering::Acquire)
        || app.incoming.load(Ordering::Acquire) > 0
        || !app.scheduler.lock().unwrap().active.is_empty()
        || !app.store.active_jobs()?.is_empty()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "処理・待機・転送・更新・モデル操作中は削除できません。".into(),
        ));
    }
    let target = app.clone();
    let job_id = id.to_owned();
    tokio::task::spawn_blocking(move || target.store.delete_job(&job_id, &client_id))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "削除処理を完了できませんでした。".into(),
            )
        })??;
    Ok(json!({"deleted":true,"id":id,"jobId":id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[tokio::test]
    async fn single_job_http_delete_is_authenticated_attributed_idle_and_scoped() {
        let root = std::env::temp_dir().join(format!("tc-delete-{}", uuid::Uuid::new_v4()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let runtime = crate::config::Runtime {
            config: crate::config::Config {
                host: "127.0.0.1".into(),
                port,
                lan: false,
                instance_id: "delete-fixture".into(),
                data_dir: root.join("data"),
                whisper_worker: Some(root.join("missing.exe")),
                ..Default::default()
            },
            root: root.clone(),
            home: root.join("settings"),
            installed: false,
            process_record: None,
            open: false,
        };
        let app = App::new(runtime).await.unwrap();
        let make = || {
            app.store.create(json!({"status":"running","phase":"transcribing","clientId":"transcript-cutter","segmentCount":0,"metadata":{}})).unwrap()
        };
        let target = make();
        let id = target["id"].as_str().unwrap();
        app.store
            .append(
                id,
                json!({"start":0.,"end":1.,"text":"fixture","speaker":"1"}),
            )
            .unwrap();
        app.store
            .edit(id, &[json!({"id":1,"text":"edited fixture"})])
            .unwrap();
        app.store.update(id, json!({"status":"complete"})).unwrap();
        let other = make();
        let other_id = other["id"].as_str().unwrap();
        app.store
            .update(other_id, json!({"status":"complete"}))
            .unwrap();
        let original = root.join("original.mp4");
        std::fs::write(&original, b"original fixture").unwrap();
        std::fs::hard_link(&original, app.store.dir.join(id).join("source.media")).unwrap();
        std::fs::write(
            app.store.dir.join(other_id).join("source.media"),
            b"other fixture",
        )
        .unwrap();
        let model = root.join("model.bin");
        std::fs::write(&model, b"model fixture").unwrap();
        let target_app = app.clone();
        let stop = app.stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                crate::http::router(target_app)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap()
        });
        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let endpoint = format!("{url}/api/jobs/{id}/delete");
        let body = json!({"confirm":"delete-job","clientId":"transcript-cutter"});
        assert_eq!(
            client
                .post(&endpoint)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let post = |value: Value| {
            client
                .post(&endpoint)
                .header("X-Local-Token", &app.token)
                .json(&value)
        };
        assert_eq!(
            post(json!({})).send().await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(json!({"confirm":"delete-job","clientId":"other"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let pending = make();
        assert_eq!(
            post(body.clone()).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        app.store
            .update(
                pending["id"].as_str().unwrap(),
                json!({"status":"cancelled"}),
            )
            .unwrap();
        app.incoming.store(1, Ordering::Release);
        assert_eq!(
            post(body.clone()).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        app.incoming.store(0, Ordering::Release);
        app.maintenance.store(true, Ordering::Release);
        assert_eq!(
            post(body.clone()).send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        app.maintenance.store(false, Ordering::Release);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let locked = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(app.store.dir.join(id).join("source.media"))
                .unwrap();
            assert_eq!(
                post(body.clone()).send().await.unwrap().status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
            assert_eq!(
                app.store.segments(id, 0, 10).unwrap()["segments"][0]["text"],
                "edited fixture"
            );
            assert!(app.store.dir.join(id).join("source.media").is_file());
            drop(locked);
        }
        assert_eq!(app.health()["capabilities"]["jobDeletion"], true);
        let response = post(body.clone()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap()["jobId"], id);
        assert!(app.store.get(id).is_err());
        assert!(!app.store.dir.join(id).exists());
        assert_eq!(
            client
                .get(format!("{url}/api/jobs/{id}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(body).send().await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        assert!(app.store.get(other_id).is_ok());
        assert_eq!(
            std::fs::read(app.store.dir.join(other_id).join("source.media")).unwrap(),
            b"other fixture"
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"original fixture");
        assert_eq!(std::fs::read(&model).unwrap(), b"model fixture");
        let data: PathBuf = app.store.dir.clone();
        app.stop.cancel();
        server.await.unwrap();
        drop(app);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reopened = crate::store::Store::open(&data).unwrap();
        assert!(reopened.get(id).is_err());
        assert!(reopened.get(other_id).is_ok());
        drop(reopened);
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
