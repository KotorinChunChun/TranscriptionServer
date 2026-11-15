use crate::{app::App, config, http};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

async fn wait(app: &App, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = app.store.get(id).unwrap();
            if !crate::store::active(&job) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn auto_gpu_success_partial_failure_cpu_fallback_cancel_and_restart_busy() {
    let dir = std::env::temp_dir().join(format!("tc-auto-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("fixture.exe");
    let csc = PathBuf::from(std::env::var_os("WINDIR").unwrap())
        .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    let built = crate::platform::command(&csc)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "/nologo",
            "/r:System.Web.Extensions.dll",
            &format!("/out:{}", exe.display()),
            "tests\\fixtures\\auto-device-runtime.cs",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stdout)
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = App::new(config::Runtime {
        config: config::Config {
            host: "127.0.0.1".into(),
            port,
            lan: false,
            data_dir: dir.join("data"),
            whisper_worker: Some(exe.clone()),
            ffmpeg: Some(exe.clone()),
            ffprobe: Some(exe),
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
    let upload = |mode: &str, device: &str| {
        client.post(format!("{url}/api/jobs?engine=whisper&model=tiny&device={device}&diarize=false&name=test.wav")).header("X-Local-Token",&app.token).body(mode.to_owned())
    };
    let management: Value = client
        .get(format!("{url}/api/management"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let models = management["models"].as_array().unwrap();
    let unsupported = models.iter().find(|m| m["id"] == "whisper/base").unwrap();
    assert_eq!(unsupported["supported"], false);
    assert_eq!(unsupported["loadable"], false);
    assert_eq!(unsupported["installable"], false);
    let tiny = models.iter().find(|m| m["id"] == "whisper/tiny").unwrap();
    assert_eq!(tiny["supported"], true);
    assert_eq!(tiny["available"], true);
    assert_eq!(tiny["modelInstalled"], true);
    let haas = models
        .iter()
        .find(|m| m["id"] == "haas/haas-ja-v1.0")
        .unwrap();
    assert_eq!(haas["supported"], true);
    assert_eq!(haas["runtimeInstallable"], true);
    assert_eq!(haas["available"], false);
    for (mode, device, status, actual, fallback) in [
        ("normal", "auto", "complete", "cuda", false),
        ("gpu-fail", "auto", "complete", "cpu", true),
        ("partial-fail", "auto", "complete", "cpu", true),
        ("always-fail", "auto", "error", "cpu", true),
        ("gpu-fail", "gpu", "error", "cuda", false),
        ("normal", "cpu", "complete", "cpu", false),
    ] {
        let response = upload(mode, device).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        let accepted: Value = response.json().await.unwrap();
        use sha2::{Digest, Sha256};
        assert_eq!(
            accepted["job"]["sourceSha256"],
            hex::encode(Sha256::digest(mode.as_bytes()))
        );
        assert_eq!(accepted["job"]["sourceBytes"], mode.len());
        if mode == "normal" && device == "cpu" {
            assert_eq!(
                accepted["job"]["metadata"]["mediaDeduplicationApplied"],
                true
            );
        }
        let id = accepted["job"]["id"].as_str().unwrap();
        if mode == "partial-fail" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while app.store.get(id).unwrap()["metadata"]["gpuAttemptSegmentCount"] != 1 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                app.store.segments(id, 0, 10).unwrap()["segments"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let done = wait(&app, id).await;
        assert_eq!(done["status"], status, "{done}");
        assert_eq!(done["metadata"]["actualDevice"], actual);
        assert_eq!(done["metadata"]["gpuFallback"] == true, fallback);
        assert!(done["metadata"]["runSeconds"].as_f64().unwrap() > 0.);
        let rows = app.store.segments(id, 0, 10).unwrap();
        if status == "complete" {
            assert_eq!(rows["total"], 2);
            assert_eq!(rows["segments"][0]["id"], 1);
            assert_eq!(rows["segments"][1]["id"], 2);
            assert_eq!(
                rows["segments"][0]["text"],
                if actual == "cpu" {
                    "cpu transcript"
                } else {
                    "gpu transcript"
                }
            );
        }
        if fallback {
            let stages = done["metadata"]["measurements"]["stages"]
                .as_array()
                .unwrap();
            assert!(stages.iter().any(|s| s["stage"] == "worker_processing"
                && s["executionDevice"] == "gpu"
                && s["status"] == "error"));
            assert!(stages.iter().any(|s| s["stage"] == "worker_processing"
                && s["executionDevice"] == "cpu"
                && s["cpuFallbackAttempt"] == true));
            assert!(
                done["metadata"]["gpuFallbackReason"]
                    .as_str()
                    .unwrap()
                    .contains("CUDA")
            );
            assert!(done["metadata"]["gpuAttemptSeconds"].as_f64().unwrap() > 0.);
        }
        if status == "error" && fallback {
            assert!(done["error"].as_str().unwrap().contains("CPU試行"));
            assert_eq!(rows["total"], 0);
        }
    }
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper/tiny")
        .text("device", "cpu")
        .text("diarize", "false")
        .text("response_format", "verbose_json")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"normal".to_vec()).file_name("test.wav"),
        );
    let result = client
        .post(format!("{url}/v1/audio/transcriptions"))
        .header("X-Local-Token", &app.token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        result.status(),
        reqwest::StatusCode::OK,
        "{}",
        result.text().await.unwrap()
    );
    let records = app.store.list(Some("openai-api"), None).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["sourceBytes"], 6);
    assert_eq!(records[0]["metadata"]["mediaDeduplicationApplied"], true);
    let accepted: Value = upload("slow", "auto")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = accepted["job"]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.store.get(id).unwrap()["metadata"]["gpuAttemptSegmentCount"] != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let restart = || {
        client
            .post(format!("{url}/api/management/restart"))
            .header("X-Local-Token", &app.token)
            .json(&json!({"confirm":"restart-server"}))
    };
    assert_eq!(
        restart().send().await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    assert!(
        !app.restart_requested
            .load(std::sync::atomic::Ordering::Acquire)
    );
    client
        .post(format!("{url}/api/jobs/{id}/cancel"))
        .header("X-Local-Token", &app.token)
        .send()
        .await
        .unwrap();
    let cancelled = wait(&app, id).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_ne!(cancelled["metadata"]["gpuFallback"], true);
    assert_eq!(app.store.segments(id, 0, 10).unwrap()["total"], 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.running.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::write(dir.join("preload-fail"), b"1").unwrap();
    crate::models::action(
        app.clone(),
        json!({"action":"load","model":"whisper/tiny","device":"auto"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.maintenance.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(app.management.lock().unwrap()["state"], "complete");
    assert_eq!(app.management.lock().unwrap()["gpuFallback"], true);
    assert_eq!(app.slots[0].lock().await.as_ref().unwrap().device, "cpu");
    std::fs::write(dir.join("no-gpu"), b"1").unwrap();
    crate::models::refresh(&app).await;
    let answer: Value = upload("normal", "auto")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let done = wait(&app, answer["job"]["id"].as_str().unwrap()).await;
    assert_eq!(done["metadata"]["executionDevice"], "cpu");
    assert_eq!(done["metadata"]["actualDevice"], "cpu");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.running.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client
            .post(format!("{url}/api/management/restart"))
            .json(&json!({"confirm":"restart-server"}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(format!("{url}/api/management/restart"))
            .header("X-Local-Token", &app.token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    assert!(restart().send().await.unwrap().status().is_success());
    assert!(
        app.restart_requested
            .load(std::sync::atomic::Ordering::Acquire)
    );
    server.await.unwrap();
    drop(app);
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::remove_dir_all(dir).unwrap();
}
