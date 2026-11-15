//! Prepare execution programs and the explicitly selected initial models before admission.
use crate::{app::App, catalog::Tools, config, runtime_install};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{future::Future, pin::Pin, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) type Installer = for<'a> fn(
    &'a App,
    &'a str,
    &'a CancellationToken,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

pub(crate) fn install_runtime<'a>(
    app: &'a App,
    id: &'a str,
    stop: &'a CancellationToken,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if id.contains('/') {
            crate::models::operate(app, "install", id, 0, "cpu", stop).await
        } else {
            runtime_install::install(app, id, stop).await
        }
    })
}

pub async fn prepare(app: &App, install: Installer) -> Result<()> {
    if !app.runtime.config.auto_install_runtimes {
        *app.runtime_setup.lock().unwrap() = json!({"state":"disabled"});
        return Ok(());
    }
    // Another instance sharing this home must not prepare the same installation concurrently.
    std::fs::create_dir_all(&app.runtime.home)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(app.runtime.home.join(".runtime-setup.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        *app.runtime_setup.lock().unwrap() =
            json!({"state":"incomplete","message":"別のサーバーが実行環境を準備中です。"});
        return Ok(());
    }
    let before = runtime_install::inventory(app);
    let preparation_cancel = app.management_cancel.lock().unwrap().clone();
    let mut results = Vec::new();
    for row in before.as_array().into_iter().flatten() {
        if app.stop.is_cancelled() || preparation_cancel.is_cancelled() {
            bail!("実行環境の初回準備を中止しました。")
        }
        let id = row["id"].as_str().context("実行環境IDがありません")?;
        let label = row["label"].as_str().unwrap_or(id);
        if row["healthy"] == true || row["restartRequired"] == true {
            results.push(json!({"id":id,"state":"reused"}));
            continue;
        }
        // Healthy external programs are reusable; broken external paths are never overwritten.
        if row["installable"] != true {
            results.push(json!({"id":id,"state":"error","message":row["statusMessage"].as_str().filter(|s| !s.is_empty()).unwrap_or("自動導入できません。配置と依存ライブラリを確認してください。")}));
            continue;
        }
        println!("実行環境の初回準備: {label}を導入中（モデル本体は取得しません）");
        *app.management.lock().unwrap() =
            json!({"state":"running","action":"install-runtime","model":id,"targetLabel":label});
        let stop = preparation_cancel.child_token();
        let result = {
            let work = tokio::time::timeout(Duration::from_secs(3600), install(app, id, &stop));
            tokio::pin!(work);
            let mut timer = tokio::time::interval(Duration::from_secs(2));
            let mut last = String::new();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => bail!("実行環境の初回準備を中止しました。"),
                    _ = app.stop.cancelled() => bail!("サーバーの終了により初回準備を中止しました。"),
                    result = &mut work => break result.context("実行環境の準備がタイムアウトしました").and_then(|v| v),
                    _ = timer.tick() => {
                        let progress = app.management.lock().unwrap().clone();
                        let current = progress["receivedBytes"].as_u64();
                        let total = progress["totalBytes"].as_u64().filter(|v| *v > 0);
                        let text = match (current, total) {
                            (Some(n), Some(total)) if progress["phase"] == "download" => format!("{:.0}%", n as f64 / total as f64 * 100.0),
                            _ => progress["message"].as_str().unwrap_or("準備中").to_string(),
                        };
                        if text != last { println!("  {label}: {text}"); last = text; }
                    }
                }
            }
        };
        match result {
            Ok(()) => results.push(json!({"id":id,"state":"installed"})),
            Err(error) => {
                let message = error.to_string().chars().take(2000).collect::<String>();
                println!("  {label}: 導入できませんでした。{message}");
                results.push(json!({"id":id,"state":"error","message":message}));
            }
        }
    }
    if app.stop.is_cancelled() {
        bail!("実行環境の初回準備を中止しました。")
    }
    activate(app).await?;
    let after = runtime_install::inventory(app);
    let complete = after
        .as_array()
        .is_some_and(|rows| rows.iter().all(|r| r["healthy"] == true));
    let initial_models = prepare_models(app, install, &preparation_cancel).await?;
    let models_complete = initial_models["state"] == "complete";
    let setup = json!({"state":if complete && models_complete {"complete"} else {"incomplete"},
        "results":results,"version":config::VERSION,"finishedAt":crate::store::now(),
        "modelDownloads":true,"initialModels":initial_models});
    *app.runtime_setup.lock().unwrap() = setup.clone();
    *app.management.lock().unwrap() = json!({"state":"idle","message":""});
    if let Err(error) = config::write_json(&app.runtime.home.join("runtime-setup.json"), &setup) {
        eprintln!("実行環境の準備記録を保存できませんでした: {error}");
    }
    println!(
        "実行環境の初回準備: {}",
        if complete && models_complete {
            "完了（手動再起動不要）"
        } else {
            "一部未完了。管理画面で状態を確認してください。"
        }
    );
    Ok(())
}

pub(crate) const INITIAL_MODELS: [&str; 3] = [
    "auxiliary/nemotron-3-diarization",
    "auxiliary/silero-vad",
    "whisper/base",
];

async fn prepare_models(
    app: &App,
    install: Installer,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    let path = app.runtime.home.join("initial-models.json");
    // A successful entry records initial provisioning, not a policy to reinstall
    // models that an administrator deliberately removes later.
    let mut receipt = if path.try_exists()? {
        let value: serde_json::Value = config::read_json(&path)?;
        if value["schemaVersion"] != 1 || !value["completed"].is_object() {
            bail!("初期モデル導入記録が不正です。initial-models.jsonを確認してください。")
        }
        value
    } else {
        json!({"schemaVersion":1,"completed":{}})
    };
    *app.runtime_setup.lock().unwrap() = json!({"state":"preparing","modelDownloads":true});
    let mut results = Vec::new();
    for id in INITIAL_MODELS {
        if app.stop.is_cancelled() || cancel.is_cancelled() {
            bail!("初期モデルの導入を中止しました。")
        }
        if receipt["completed"][id] == true {
            results.push(json!({"id":id,"state":"previously_completed"}));
            continue;
        }
        crate::models::refresh(app).await;
        let installed = app
            .model_inventory
            .lock()
            .unwrap()
            .as_array()
            .is_some_and(|rows| {
                rows.iter()
                    .any(|r| r["id"] == id && r["modelInstalled"] == true)
            });
        *app.management.lock().unwrap() = json!({"state":"running","action":"install","model":id,
            "targets":INITIAL_MODELS,"totalCount":3,"completedCount":results.len(),"message":"初期モデルを導入中"});
        let result = if installed {
            Ok(())
        } else {
            tokio::select! {
                _ = app.stop.cancelled() => bail!("初期モデルの導入を中止しました。"),
                _ = cancel.cancelled() => bail!("初期モデルの導入を中止しました。"),
                result = tokio::time::timeout(Duration::from_secs(3600), install(app, id, cancel)) =>
                    result.context("初期モデルの導入がタイムアウトしました").and_then(|v| v),
            }
        };
        if app.stop.is_cancelled() || cancel.is_cancelled() {
            bail!("初期モデルの導入を中止しました。")
        }
        let (state, message) = match result {
            Ok(()) => {
                receipt["completed"][id] = true.into();
                config::write_json(&path, &receipt)?;
                ("complete", format!("{id}の初期導入が完了しました。"))
            }
            Err(error) => ("error", error.to_string().chars().take(2000).collect()),
        };
        app.management_results.lock().unwrap()[id] =
            json!({"state":state,"action":"install","message":message});
        results.push(json!({"id":id,"state":state,"message":message}));
    }
    crate::models::refresh(app).await;
    let complete = INITIAL_MODELS
        .iter()
        .all(|id| receipt["completed"][*id] == true);
    Ok(json!({"state":if complete {"complete"} else {"incomplete"},"results":results}))
}

/// Called while admission is fenced by maintenance. No recognition uses changing programs.
pub async fn activate(app: &App) -> Result<()> {
    let pending = app.runtime_pending_restart.lock().unwrap().clone();
    if pending.contains("whisper") {
        for slot in &app.slots {
            let mut session = slot.lock().await;
            if let Some(worker) = session.as_mut() {
                worker.unload().await?;
            }
            *session = None;
        }
    }
    if pending.contains("speech") {
        for slot in &app.native_slots {
            *slot.lock().await = None;
        }
    }
    app.replace_tools(Tools::resolve(&app.runtime));
    app.runtime_pending_restart.lock().unwrap().clear();
    crate::models::refresh(app).await;
    let inventory = runtime_install::inventory(app);
    let mut setup = app.runtime_setup.lock().unwrap();
    if setup["state"] != "preparing" {
        setup["state"] = if inventory
            .as_array()
            .is_some_and(|rows| rows.iter().all(|r| r["healthy"] == true))
        {
            "complete"
        } else {
            "incomplete"
        }
        .into();
        for result in setup["results"].as_array_mut().into_iter().flatten() {
            if inventory.as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|r| r["id"] == result["id"] && r["healthy"] == true)
            }) {
                *result = json!({"id":result["id"],"state":"installed"});
            }
        }
    }
    Ok(())
}
