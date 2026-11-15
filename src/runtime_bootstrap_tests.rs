use crate::{
    app::App,
    config::{Config, Runtime},
    platform,
    runtime_bootstrap::Installer,
    runtime_install,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

fn fixture_install<'a>(
    app: &'a App,
    id: &'a str,
    _: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        fs::create_dir_all(&app.runtime.home)?;
        use std::io::Write;
        if id.contains('/') {
            writeln!(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(app.runtime.home.join("model-installations.txt"))?,
                "{id}"
            )?;
            if app.runtime.home.join("fail-initial-model").exists() {
                bail!("fixture model unavailable")
            }
            return Ok(());
        }
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(app.runtime.home.join("installations.txt"))?,
            "{id}"
        )?;
        if app.runtime.home.join(format!("fail-{id}")).exists() {
            bail!("fixture unavailable")
        }
        if app.runtime.home.join("cancel").exists() {
            app.stop.cancel();
            bail!("cancelled")
        }
        let p = runtime_install::package(id)?;
        let root = runtime_install::target(&app.runtime.home, p);
        fs::create_dir_all(&root)?;
        let mut programs = json!({});
        for (name, _) in p.programs {
            let exe = format!("{name}.exe");
            fs::copy(app.runtime.root.join("fixture.exe"), root.join(&exe))?;
            programs[name] = exe.into();
        }
        let mut receipt =
            json!({"id":id,"version":p.version,"archiveSha256":p.sha256,"programs":programs});
        if id == "whisper" {
            let worker = b"fixture only";
            fs::write(root.join("whisper_worker.py"), worker)?;
            receipt["workerSha256"] = format!("{:x}", Sha256::digest(worker)).into();
        }
        crate::config::write_json(&root.join("receipt.json"), &receipt)?;
        app.runtime_pending_restart
            .lock()
            .unwrap()
            .insert(id.into());
        Ok(())
    })
}

fn runtime(root: &Path, name: &str) -> Runtime {
    Runtime {
        config: Config {
            instance_id: "bootstrap-fixture".into(),
            data_dir: root.join(name).join("data"),
            ..Default::default()
        },
        root: root.into(),
        home: root.join(name),
        installed: false,
        process_record: None,
        open: false,
    }
}
async fn close(app: Arc<App>) {
    app.stop.cancel();
    for _ in 0..100 {
        if Arc::strong_count(&app) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn paused_install<'a>(
    app: &'a App,
    id: &'a str,
    stop: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        while !app.runtime.home.join("continue").exists() {
            tokio::select! { _=stop.cancelled()=>bail!("cancelled"), _=tokio::time::sleep(std::time::Duration::from_millis(10))=>{} }
        }
        fixture_install(app, id, stop).await
    })
}
#[tokio::test]
async fn startup_prepares_tools_and_initial_models_without_manual_restart() {
    let root = std::env::temp_dir().join(format!("tc-bootstrap-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let csc = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap())
        .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
    let output = platform::command(&csc)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "/nologo",
            &format!("/out:{}", root.join("fixture.exe").display()),
            "tests\\fixtures\\bootstrap-runtime.cs",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let installer = Some(fixture_install as Installer);
    let background = App::start_with(
        runtime(&root, "background"),
        CancellationToken::new(),
        paused_install,
    )
    .await
    .unwrap();
    let early = background.health();
    assert_eq!(early["startupState"], "preparing");
    assert_eq!(early["acceptingJobs"], false);
    assert_eq!(early["ready"], false);
    assert!(
        background
            .create(&std::collections::HashMap::new())
            .await
            .is_err()
    );
    fs::write(background.runtime.home.join("continue"), "").unwrap();
    for _ in 0..500 {
        if !background.preparing() {
            break;
        };
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        background.runtime_setup.lock().unwrap()["state"],
        "complete"
    );
    assert_eq!(background.health()["processId"], early["processId"]);
    close(background).await;
    let app = App::initialize(runtime(&root, "ready"), installer, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(app.runtime.home.join("installations.txt")).unwrap(),
        "whisper\nspeech\nnemo\nffmpeg\n"
    );
    assert!(
        runtime_install::inventory(&app)
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["healthy"] == true)
    );
    assert!(
        app.tools().whisper.is_some()
            && app.tools().ffmpeg.is_some()
            && app.tools().speech.is_some()
            && app.tools().nemo.is_some()
    );
    assert!(app.runtime_pending_restart.lock().unwrap().is_empty());
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "complete");
    assert_eq!(app.runtime_setup.lock().unwrap()["modelDownloads"], true);
    let initial_calls =
        fs::read_to_string(app.runtime.home.join("model-installations.txt")).unwrap();
    assert_eq!(
        initial_calls.lines().collect::<Vec<_>>(),
        crate::runtime_bootstrap::INITIAL_MODELS
    );
    assert_eq!(
        app.runtime_setup.lock().unwrap()["initialModels"]["state"],
        "complete"
    );
    assert!(
        app.model_inventory
            .lock()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["modelInstalled"] != true)
    );
    assert!(!app.runtime.home.join("models").exists());
    for p in &runtime_install::PACKAGES {
        let calls =
            fs::read_to_string(runtime_install::target(&app.runtime.home, p).join("calls.txt"))
                .unwrap();
        assert!(!calls.contains("pull") && !calls.contains("--download"));
    }
    close(app).await;
    let app = App::initialize(runtime(&root, "ready"), installer, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(app.runtime.home.join("installations.txt"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    assert!(
        app.runtime_setup.lock().unwrap()["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["state"] == "reused")
    );
    // The fixture deliberately has no weights. Successful initial provisioning
    // must not redownload after an administrator subsequently deletes a model.
    assert_eq!(
        fs::read_to_string(app.runtime.home.join("model-installations.txt")).unwrap(),
        initial_calls
    );
    close(app).await;

    let failing = runtime(&root, "model-failure");
    fs::create_dir_all(&failing.home).unwrap();
    fs::write(failing.home.join("fail-initial-model"), "").unwrap();
    let app = App::initialize(failing, installer, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "incomplete");
    assert!(!app.runtime.home.join("initial-models.json").exists());
    fs::remove_file(app.runtime.home.join("fail-initial-model")).unwrap();
    close(app).await;
    let app = App::initialize(
        runtime(&root, "model-failure"),
        installer,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        app.runtime_setup.lock().unwrap()["initialModels"]["state"],
        "complete"
    );
    assert_eq!(
        fs::read_to_string(app.runtime.home.join("model-installations.txt"))
            .unwrap()
            .lines()
            .count(),
        6
    );
    let receipt = app.runtime.home.join("initial-models.json");
    fs::write(&receipt, b"broken").unwrap();
    close(app).await;
    let failed = App::initialize(
        runtime(&root, "model-failure"),
        installer,
        CancellationToken::new(),
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(fs::read(receipt).unwrap(), b"broken");

    fs::create_dir_all(root.join("partial")).unwrap();
    fs::write(root.join("partial/fail-speech"), "").unwrap();
    let app = App::initialize(
        runtime(&root, "partial"),
        installer,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "incomplete");
    assert!(
        app.tools().speech.is_none() && app.tools().nemo.is_some() && app.tools().ffmpeg.is_some()
    );
    let report: Value =
        crate::config::read_json(&app.runtime.home.join("runtime-setup.json")).unwrap();
    assert!(
        report["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "speech" && r["message"] == "fixture unavailable")
    );
    close(app).await;

    fs::remove_file(root.join("partial/fail-speech")).unwrap();
    let app = App::initialize(
        runtime(&root, "partial"),
        installer,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(app.runtime.home.join("installations.txt")).unwrap(),
        "whisper\nspeech\nnemo\nffmpeg\nspeech\n"
    );
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "complete");
    close(app).await;

    let mut external = runtime(&root, "external");
    fs::copy(root.join("fixture.exe"), root.join("broken.exe")).unwrap();
    external.config.speech = Some(root.join("broken.exe"));
    let app = App::initialize(external, installer, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !fs::read_to_string(app.runtime.home.join("installations.txt"))
            .unwrap()
            .lines()
            .any(|s| s == "speech")
    );
    assert_eq!(
        fs::read(root.join("broken.exe")).unwrap(),
        fs::read(root.join("fixture.exe")).unwrap()
    );
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "incomplete");
    close(app).await;

    let mut disabled = runtime(&root, "disabled");
    disabled.config.auto_install_runtimes = false;
    let app = App::initialize(disabled, installer, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(app.runtime_setup.lock().unwrap()["state"], "disabled");
    assert!(!app.runtime.home.join("installations.txt").exists());
    close(app).await;

    fs::create_dir_all(root.join("cancelled")).unwrap();
    fs::write(root.join("cancelled/cancel"), "").unwrap();
    assert!(
        App::initialize(
            runtime(&root, "cancelled"),
            installer,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(root.join("cancelled/installations.txt")).unwrap(),
        "whisper\n"
    );
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    fs::remove_dir_all(root).unwrap();
}
