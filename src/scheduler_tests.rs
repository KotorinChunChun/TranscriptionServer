use crate::{app::App, config, http, scheduler};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("isolated scheduler condition timed out");
}
async fn fixture(installed: bool) -> (Arc<App>, PathBuf, String, tokio::task::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!("tc-concurrent-{}", uuid::Uuid::new_v4()));
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
            "tests\\fixtures\\concurrent-runtime.cs",
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
            execution: scheduler::Settings {
                ram_limit_mib: 65536,
                vram_limit_mib: 65536,
                max_gpu_jobs: 10,
                ..Default::default()
            },
            ..Default::default()
        },
        root: dir.clone(),
        home: dir.join("settings"),
        installed,
        process_record: None,
        open: false,
    })
    .await
    .unwrap();
    app.scheduler.lock().unwrap().test_memory = Some((131072, 131072));
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
    (app, dir, format!("http://127.0.0.1:{port}"), server)
}
async fn submit(
    client: &reqwest::Client,
    url: &str,
    app: &App,
    mode: &str,
    lane: &str,
    model: &str,
    device: &str,
) -> String {
    let response=client.post(format!("{url}/api/jobs?engine=whisper&model={model}&device={device}&diarize=false&workload={lane}&clientId={mode}"))
        .header("X-Local-Token",&app.token).body(mode.to_owned()).send().await.unwrap();
    if !response.status().is_success() {
        panic!(
            "submit {mode}: {} {}",
            response.status(),
            response.text().await.unwrap()
        );
    }
    response.json::<Value>().await.unwrap()["job"]["id"]
        .as_str()
        .unwrap()
        .into()
}
async fn shutdown(app: Arc<App>, dir: PathBuf, server: tokio::task::JoinHandle<()>) {
    app.stop.cancel();
    server.await.unwrap();
    until(|| app.scheduler.lock().unwrap().active.is_empty()).await;
    until(|| Arc::strong_count(&app) == 1).await;
    app.supervisor.terminate().unwrap();
    drop(app);
    // This directory was created by fixture() and contains no real models/data.
    std::fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn unknown_gpu_capacity_falls_back_only_auto_and_cpu_still_respects_ram() {
    let (app, dir, url, server) = fixture(false).await;
    {
        let mut state = app.scheduler.lock().unwrap();
        state.detected_vram_mib = 0;
        state.settings.vram_limit_mib = 0;
        state.settings.ram_limit_mib = 512;
    }
    let client = reqwest::Client::new();
    let auto = submit(&client, &url, &app, "capacity-auto", "file", "tiny", "auto").await;
    until(|| app.store.get(&auto).unwrap()["phase"] == "waiting_resources").await;
    let job = app.store.get(&auto).unwrap();
    assert_eq!(job["device"], "auto");
    assert_eq!(job["metadata"]["executionDevice"], "cpu");
    assert_eq!(
        job["metadata"]["gpuFallbackReasonCode"],
        "gpu_capacity_unknown"
    );
    assert!(!dir.join("capacity-auto.started").exists());
    app.scheduler.lock().unwrap().settings.ram_limit_mib = 65536;
    until(|| dir.join("capacity-auto.started").exists()).await;
    std::fs::write(dir.join("release"), b"go").unwrap();
    until(|| app.store.get(&auto).unwrap()["status"] == "complete").await;
    assert_eq!(
        app.store.get(&auto).unwrap()["metadata"]["actualDevice"],
        "cpu"
    );
    let gpu = submit(
        &client,
        &url,
        &app,
        "capacity-explicit",
        "file",
        "tiny",
        "gpu",
    )
    .await;
    until(|| app.store.get(&gpu).unwrap()["phase"] == "waiting_resources").await;
    assert_eq!(
        app.store.get(&gpu).unwrap()["metadata"]["executionDevice"],
        "gpu"
    );
    assert!(!dir.join("capacity-explicit.started").exists());
    app.cancel(&gpu).unwrap();
    shutdown(app, dir, server).await;
}
#[tokio::test]
async fn drain_keeps_accepted_queue_running_rejects_new_work_and_can_cancel_or_stop() {
    let (app, dir, url, server) = fixture(false).await;
    {
        let mut s = app.scheduler.lock().unwrap();
        s.settings.max_jobs = 2;
        s.settings.realtime_reserved = 1;
    }
    let client = reqwest::Client::new();
    let a = submit(&client, &url, &app, "drain-a", "file", "tiny", "cpu").await;
    let b = submit(&client, &url, &app, "drain-b", "file", "tiny", "cpu").await;
    until(|| dir.join("drain-a.started").exists()).await;
    let rejected = client
        .post(format!("{url}/api/admin/drain"))
        .json(&json!({"action":"hold","confirm":"drain-server"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 403);
    let response = client
        .post(format!("{url}/api/admin/drain"))
        .header("X-Local-Token", &app.token)
        .json(&json!({"action":"hold","confirm":"drain-server"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let ticket = response.json::<Value>().await.unwrap();
    assert!(!app.health()["acceptingJobs"].as_bool().unwrap());
    let rejected = client
        .post(format!(
            "{url}/api/jobs?engine=whisper&model=tiny&device=cpu&diarize=false"
        ))
        .header("X-Local-Token", &app.token)
        .body("must-not-create")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 503);
    assert_eq!(rejected.headers()["retry-after"], "5");
    assert_eq!(app.store.list(None, None).unwrap().len(), 2);
    let cancel = client
        .post(format!("{url}/api/admin/drain/cancel"))
        .header("X-Local-Token", &app.token)
        .json(&json!({"id":ticket["id"]}))
        .send()
        .await
        .unwrap();
    assert!(cancel.status().is_success());
    until(|| !app.draining.load(std::sync::atomic::Ordering::Acquire)).await;
    let ticket = client
        .post(format!("{url}/api/admin/drain"))
        .header("X-Local-Token", &app.token)
        .json(&json!({"action":"hold","confirm":"drain-server"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    std::fs::write(dir.join("release"), "").unwrap();
    until(|| app.drain_state.lock().unwrap()["state"] == "ready").await;
    for id in [&a, &b] {
        assert_eq!(app.store.get(id).unwrap()["status"], "complete");
    }
    assert!(!app.stop.is_cancelled());
    let response = client
        .post(format!("{url}/api/admin/drain/cancel"))
        .header("X-Local-Token", &app.token)
        .json(&json!({"id":ticket["id"]}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(!app.maintenance.load(std::sync::atomic::Ordering::Acquire));
    {
        let mut drain = crate::lifecycle::Drain::begin(&app, "update", false)
            .await
            .unwrap();
        drain.wait().await.unwrap();
        let id = app.drain_state.lock().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        crate::lifecycle::cancel(&app, &id).await.unwrap();
        assert!(drain.seal().await.is_err());
    }
    assert!(!app.draining.load(std::sync::atomic::Ordering::Acquire));
    let c = submit(&client, &url, &app, "drain-c", "realtime", "tiny", "cpu").await;
    let response = client
        .post(format!("{url}/api/admin/drain"))
        .header("X-Local-Token", &app.token)
        .json(&json!({"action":"shutdown","confirm":"drain-server"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    until(|| app.stop.is_cancelled()).await;
    assert_eq!(app.store.get(&c).unwrap()["status"], "complete");
    shutdown(app, dir, server).await;
}
#[tokio::test]
async fn ten_http_jobs_overlap_isolate_results_cancel_and_keep_realtime_range_limit() {
    let (app, dir, url, server) = fixture(false).await;
    let client = reqwest::Client::new();
    let mut jobs = vec![];
    for i in 0..10 {
        let mode = format!("job{i}");
        let model = if i % 2 == 0 { "tiny" } else { "base" };
        let device = if i % 3 == 0 { "gpu" } else { "cpu" };
        let id = submit(
            &client,
            &url,
            &app,
            &mode,
            if i < 8 { "file" } else { "realtime" },
            model,
            device,
        )
        .await;
        jobs.push((id, format!("{mode}/{model}/{device}")));
    }
    until(|| (0..10).all(|i| dir.join(format!("job{i}.started")).exists())).await;
    assert_eq!(app.scheduler.lock().unwrap().active.len(), 10);
    let catalog = client
        .get(format!("{url}/api/models"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "whisper/tiny")
            .unwrap()["execution"]["running"],
        5
    );
    let response = client
        .post(format!("{url}/api/management/scheduling"))
        .header("X-Local-Token", &app.token)
        .json(&scheduler::Settings::default())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let extra = submit(&client, &url, &app, "extra", "file", "tiny", "cpu").await;
    until(|| app.store.get(&extra).unwrap()["phase"] == "waiting_resources").await;
    assert!(!dir.join("extra.started").exists());
    app.cancel(&jobs[0].0).unwrap();
    until(|| dir.join("extra.started").exists()).await;
    for (id, _) in jobs.iter().skip(1) {
        assert_eq!(app.store.get(id).unwrap()["status"], "running");
    }
    std::fs::write(dir.join("release"), b"1").unwrap();
    until(|| app.running.lock().unwrap().is_empty()).await;
    assert!(app.job_scopes.lock().unwrap().is_empty());
    assert_eq!(app.store.get(&jobs[0].0).unwrap()["status"], "cancelled");
    for (id, text) in jobs.iter().skip(1) {
        assert_eq!(app.store.get(id).unwrap()["status"], "complete");
        let rows = app.store.segments(id, 0, 10).unwrap();
        assert_eq!(rows["segments"][0]["text"], *text);
    }
    let long = submit(&client, &url, &app, "longfile", "file", "tiny", "cpu").await;
    let rt = submit(&client, &url, &app, "longrt", "realtime", "tiny", "cpu").await;
    until(|| app.running.lock().unwrap().is_empty()).await;
    assert_eq!(app.store.get(&long).unwrap()["status"], "complete");
    assert_eq!(app.store.get(&rt).unwrap()["status"], "error");
    assert!(
        app.store.get(&rt).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("30秒")
    );
    shutdown(app, dir, server).await;
}
#[tokio::test]
async fn realtime_overtakes_file_queue_and_ram_pressure_waits_until_cancel_or_capacity_returns() {
    let (app, dir, url, server) = fixture(false).await;
    {
        let mut state = app.scheduler.lock().unwrap();
        state.settings.max_jobs = 3;
        state.settings.realtime_reserved = 1;
        state.settings.max_gpu_jobs = 2;
    }
    let client = reqwest::Client::new();
    let a = submit(&client, &url, &app, "filea", "file", "tiny", "cpu").await;
    let b = submit(&client, &url, &app, "fileb", "file", "tiny", "cpu").await;
    until(|| dir.join("filea.started").exists() && dir.join("fileb.started").exists()).await;
    let c = submit(&client, &url, &app, "filec", "file", "tiny", "cpu").await;
    let rt = submit(&client, &url, &app, "rt", "realtime", "tiny", "cpu").await;
    until(|| dir.join("rt.started").exists()).await;
    assert!(!dir.join("filec.started").exists());
    let rt2 = submit(&client, &url, &app, "rt2", "realtime", "tiny", "cpu").await;
    app.cancel(&a).unwrap();
    until(|| dir.join("rt2.started").exists()).await;
    assert!(!dir.join("filec.started").exists());
    for id in [&b, &c, &rt, &rt2] {
        app.cancel(id).unwrap();
    }
    until(|| app.running.lock().unwrap().is_empty()).await;
    app.scheduler.lock().unwrap().settings.ram_limit_mib = 1024;
    let waiting = submit(&client, &url, &app, "memory", "file", "tiny", "cpu").await;
    until(|| app.store.get(&waiting).unwrap()["metadata"]["waitingReason"] == "RAM予算待ち").await;
    assert!(app.store.get(&waiting).unwrap()["metadata"]["startedAt"].is_null());
    app.scheduler.lock().unwrap().settings.ram_limit_mib = 65536;
    until(|| dir.join("memory.started").exists()).await;
    std::fs::write(dir.join("release"), b"1").unwrap();
    until(|| app.running.lock().unwrap().is_empty()).await;
    let result = app.store.get(&waiting).unwrap();
    assert_eq!(result["status"], "complete");
    assert!(result["metadata"]["resourceWaitSeconds"].as_f64().unwrap() > 0.);
    assert_eq!(
        app.status().await.unwrap()["scheduling"]["activeWorkers"],
        json!([])
    );
    shutdown(app, dir, server).await;
}

#[tokio::test]
async fn settings_require_auth_idle_valid_files_preserve_unknown_fields_and_reload() {
    use clap::Parser;
    let (app, dir, url, server) = fixture(true).await;
    let client = reqwest::Client::new();
    let desired = scheduler::Settings {
        max_jobs: 6,
        realtime_reserved: 2,
        max_gpu_jobs: 2,
        ram_limit_mib: 24576,
        vram_limit_mib: 8192,
    };
    let endpoint = format!("{url}/api/management/scheduling");
    let unauth = client.post(&endpoint).json(&desired).send().await.unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::FORBIDDEN);
    let missing = client
        .post(&endpoint)
        .header("X-Local-Token", &app.token)
        .json(&desired)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(app.scheduler.lock().unwrap().settings.max_jobs, 10);
    let settings_file = app.runtime.home.join("settings.json");
    let mut original = serde_json::to_value(&app.runtime.config).unwrap();
    original["futureSettings"] = json!({"preserve":true});
    config::write_json(&settings_file, &original).unwrap();
    let invalid = client
        .post(&endpoint)
        .header("X-Local-Token", &app.token)
        .json(&json!({"maxJobs":11}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(config::read_json(&settings_file).unwrap(), original);
    let saved = client
        .post(&endpoint)
        .header("X-Local-Token", &app.token)
        .json(&desired)
        .send()
        .await
        .unwrap();
    assert!(saved.status().is_success());
    assert_eq!(
        config::read_json(&settings_file).unwrap()["futureSettings"],
        original["futureSettings"]
    );
    let args = config::Args::try_parse_from([
        "server",
        "--instance-id",
        "parallel-test",
        "--local",
        "--port",
        "48066",
        "--settings-dir",
        app.runtime.home.to_str().unwrap(),
        "--data-dir",
        app.store.dir.to_str().unwrap(),
        "--process-record",
        dir.join("process.json").to_str().unwrap(),
    ])
    .unwrap();
    let mut restored = config::Runtime::load(args).unwrap();
    restored.installed = true;
    restored.save().unwrap();
    assert_eq!(
        config::read_json(&settings_file).unwrap()["futureSettings"],
        original["futureSettings"]
    );
    assert_eq!(
        serde_json::to_value(&restored.config.execution).unwrap(),
        serde_json::to_value(&desired).unwrap()
    );
    let result = client
        .get(format!("{url}/api/scheduling"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(result["settings"]["maxJobs"], 6);
    assert_eq!(result["settings"]["ramLimitMiB"], 24576);
    assert!(result["limits"]["ramMiB"].is_number());
    std::fs::write(&settings_file, b"{invalid").unwrap();
    let rejected = client
        .post(&endpoint)
        .header("X-Local-Token", &app.token)
        .json(&scheduler::Settings::default())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(app.scheduler.lock().unwrap().settings.max_jobs, 6);
    shutdown(app, dir, server).await;
}

#[tokio::test]
async fn manual_whisper_residency_survives_changed_thread_allocation() {
    let (app, dir, url, server) = fixture(false).await;
    crate::jobs::preload(
        &app,
        0,
        "tiny",
        "cpu",
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let mut s = app.scheduler.lock().unwrap();
        s.settings.max_jobs = 2;
        s.settings.realtime_reserved = 1;
        s.settings.max_gpu_jobs = 2;
    }
    std::fs::write(dir.join("release"), b"1").unwrap();
    let id = submit(
        &reqwest::Client::new(),
        &url,
        &app,
        "pinned",
        "file",
        "tiny",
        "cpu",
    )
    .await;
    until(|| app.running.lock().unwrap().is_empty()).await;
    assert_eq!(app.store.get(&id).unwrap()["status"], "complete");
    assert!(
        app.slots[0]
            .lock()
            .await
            .as_mut()
            .is_some_and(|s| s.pinned && s.alive())
    );
    assert!(app.job_scopes.lock().unwrap().is_empty());
    shutdown(app, dir, server).await;
}
