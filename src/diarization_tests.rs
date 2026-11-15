use crate::{app::App, catalog, config, http};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

async fn terminal(app: &App, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = app.store.get(id).unwrap();
            if !crate::store::active(&job) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture job did not finish")
}
async fn upload(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    query: &str,
    mode: &str,
) -> reqwest::Response {
    client.post(format!("{url}/api/jobs?engine=diarization&model=nemotron-3-diarization&clientId=Example-ai-room&{query}"))
        .header("X-Local-Token", token).body(mode.to_owned()).send().await.unwrap()
}
#[tokio::test]
async fn standalone_http_separates_without_asr_and_preserves_queue_cancel_unavailable() {
    let dir = std::env::temp_dir().join(format!("tc-diarization-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("fixture.exe");
    let csc = PathBuf::from(std::env::var_os("WINDIR").unwrap())
        .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    let result = crate::platform::command(&csc)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "/nologo",
            "/r:System.Web.Extensions.dll",
            &format!("/out:{}", exe.display()),
            "tests\\fixtures\\diarization-runtime.cs",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    std::fs::write(dir.join("model.gguf"), b"12345678").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let runtime = config::Runtime {
        config: config::Config {
            host: "127.0.0.1".into(),
            port,
            lan: false,
            data_dir: dir.join("data"),
            speech: Some(exe.clone()),
            ffmpeg: Some(exe.clone()),
            ffprobe: Some(exe),
            whisper_worker: Some(dir.join("missing-worker.exe")),
            ..Default::default()
        },
        root: dir.clone(),
        home: dir.join("settings"),
        installed: false,
        process_record: None,
        open: false,
    };
    let app = App::new(runtime).await.unwrap();
    {
        let mut scheduler = app.scheduler.lock().unwrap();
        scheduler.test_memory = Some((131072, 131072));
        scheduler.settings.vram_limit_mib = 65536;
    }
    let target = app.clone();
    let stop = app.stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            http::router(target).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
        .unwrap();
    });
    let url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let token = app.token.clone();
    let health: Value = client
        .get(format!("{url}/api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["protocolVersion"], 3);
    assert_eq!(health["capabilities"]["standaloneDiarization"], true);
    let models: Value = client
        .get(format!("{url}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = models["data"].as_array().unwrap();
    let model = rows
        .iter()
        .find(|r| r["id"] == "diarization/nemotron-3-diarization")
        .unwrap();
    assert_eq!(model["x_meeting"]["task"], "diarization");
    assert_eq!(model["x_meeting"]["availability"], "installed");
    assert!(rows.iter().any(|r| r["id"] == "haas/haas-ja-v1.0"));
    assert!(
        rows.iter()
            .all(|r| r["x_meeting"]["engineId"] != "nemotron-transcribe")
    );
    let unconfigured = health["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "nemotron-transcribe")
        .unwrap();
    assert_eq!(unconfigured["ready"], false);
    assert!(unconfigured["reason"].as_str().unwrap().contains("19GB"));
    assert!(
        crate::models::action(
            app.clone(),
            json!({"action":"install","model":"nemotron-transcribe/nemotron-3.5-transcribe"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("19GB")
    );
    assert_eq!(unconfigured["runtime"], "speech.cpp");
    assert!(unconfigured["models"].as_array().unwrap().is_empty());
    let response = upload(
        &client,
        &url,
        &token,
        "device=auto&start=2&end=5&name=fixture.wav",
        "normal",
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let answer: Value = response.json().await.unwrap();
    let id = answer["job"]["id"].as_str().unwrap();
    let done = terminal(&app, id).await;
    assert_eq!(done["status"], "complete");
    assert_eq!(done["metadata"]["actualDevice"], "gpu");
    assert_eq!(done["metadata"]["asrPerformed"], false);
    assert_eq!(done["metadata"]["diarizationStatus"], "complete");
    assert_eq!(done["clientId"], "Example-ai-room");
    let page: Value = client
        .get(format!("{url}/api/jobs/{id}/segments?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 2);
    let first = &page["segments"][0];
    assert_eq!(first["text"], "");
    assert_eq!(first["start"], 2.25);
    assert_eq!(first["end"], 4.5);
    assert_eq!(first["speakers"], json!(["話者1"]));
    let page: Value = client
        .get(format!("{url}/api/jobs/{id}/segments?after=1&limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["segments"][0]["start"], 3.5);
    assert_eq!(page["segments"][0]["end"], 5.0);
    assert_eq!(page["segments"][0]["speakers"], json!(["話者2"]));
    for (mode, status, diar) in [
        ("silence", "complete", "no_speakers"),
        ("invalid", "error", "error"),
    ] {
        let answer: Value = upload(&client, &url, &token, "device=cpu", mode)
            .await
            .json()
            .await
            .unwrap();
        let job = terminal(&app, answer["job"]["id"].as_str().unwrap()).await;
        assert_eq!(job["status"], status);
        assert_eq!(job["metadata"]["diarizationStatus"], diar);
        assert_eq!(job["segmentCount"], 0);
        assert!(job["metadata"]["finishedAt"].is_string());
        assert!(job["metadata"]["runSeconds"].as_f64().unwrap() > 0.);
    }
    app.scheduler.lock().unwrap().settings.max_jobs = 3;
    app.scheduler.lock().unwrap().settings.realtime_reserved = 2;
    let slow: Value = upload(&client, &url, &token, "device=cpu", "slow")
        .await
        .json()
        .await
        .unwrap();
    let slow_id = slow["job"]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(dir.join("calls.txt"))
                .unwrap_or_default()
                .lines()
                .count()
                >= 4
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let queued: Value = upload(&client, &url, &token, "device=cpu", "normal")
        .await
        .json()
        .await
        .unwrap();
    let queued_id = queued["job"]["id"].as_str().unwrap();
    assert_eq!(app.store.get(queued_id).unwrap()["status"], "queued");
    for target in [queued_id, slow_id] {
        let response = client
            .post(format!("{url}/api/jobs/{target}/cancel"))
            .header("X-Local-Token", &token)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(terminal(&app, target).await["status"], "cancelled");
    }
    app.scheduler.lock().unwrap().settings.vram_limit_mib = 1;
    let answer: Value = upload(&client, &url, &token, "device=gpu", "normal")
        .await
        .json()
        .await
        .unwrap();
    let gpu_id = answer["job"]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.store.get(gpu_id).unwrap()["phase"] != "waiting_resources" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let waiting: Value = client
        .get(format!("{url}/api/jobs/{gpu_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(waiting["job"]["metadata"]["startedAt"].is_null());
    assert_eq!(waiting["job"]["metadata"]["runSeconds"], 0.);
    assert!(
        waiting["job"]["metadata"]["resourceWaitSeconds"]
            .as_f64()
            .unwrap()
            >= 0.15
    );
    app.scheduler.lock().unwrap().settings.vram_limit_mib = 65536;
    let job = terminal(&app, gpu_id).await;
    assert_eq!(job["status"], "complete");
    assert_eq!(job["metadata"]["actualDevice"], "gpu");
    assert!(job["metadata"]["runSeconds"].as_f64().unwrap() > 0.);
    assert!(job["metadata"]["resourceWaitSeconds"].as_f64().unwrap() >= 0.15);
    assert_eq!(job["metadata"]["timingVersion"], 1);
    let response = upload(&client, &url, &token, "device=cpu&diarize=false", "normal").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let response = client
        .post(format!("{url}/v1/audio/transcriptions"))
        .header("X-Local-Token", &token)
        .multipart(
            reqwest::multipart::Form::new()
                .text("model", "diarization/nemotron-3-diarization")
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(b"normal".to_vec()).file_name("test.wav"),
                ),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let count = app.store.list(None, None).unwrap().len();
    std::fs::remove_file(dir.join("model.gguf")).unwrap();
    // Stale discovery must not allow an implicit model download.
    let response = upload(&client, &url, &token, "device=cpu", "normal").await;
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(app.store.list(None, None).unwrap().len(), count);
    crate::models::refresh(&app).await;
    assert!(
        catalog::catalog(&app.engines.lock().unwrap())["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["x_meeting"]["task"] != "diarization")
    );
    let calls = std::fs::read_to_string(dir.join("calls.txt")).unwrap();
    assert!(!calls.contains("UNEXPECTED_ASR"));
    assert!(calls.lines().all(|line| line.starts_with("diarize\t")));
    assert!(
        calls
            .lines()
            .all(|line| line.contains("--device\tcpu") || line.contains("--device\tgpu"))
    );
    app.stop.cancel();
    server.await.unwrap();
    drop(app);
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::remove_dir_all(Path::new(&dir)).unwrap();
}
