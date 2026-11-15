use crate::{app::App, catalog, catalog::Tools, platform};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

pub fn selection(id: &str) -> Result<(&str, &str)> {
    let (engine, model) = id.split_once('/').context("モデルIDを指定してください")?;
    let valid = if engine == "whisper" {
        catalog::WHISPER_MODELS.iter().any(|r| r.0 == model)
    } else if engine == crate::nemo::ENGINE {
        model == crate::nemo::MODEL
    } else if engine == "auxiliary" {
        matches!(model, "silero-vad" | "nemotron-3-diarization")
    } else {
        catalog::NATIVE
            .iter()
            .any(|r| r.0 == engine && r.2.iter().any(|m| m.0 == model))
    };
    if !valid {
        bail!("管理対象外のモデルです")
    };
    Ok((engine, model))
}
pub(crate) const RECOMMENDED_MODELS: [&str; 2] = ["haas/haas-ja-v1.0", "qwen/qwen3-asr-1.7b"];
pub async fn refresh(app: &App) {
    *app.runtime_storage.lock().unwrap() = None;
    let scope = match platform::Job::new() {
        Ok(v) => v,
        Err(_) => return,
    };
    // Pending runtimes may inspect/download weights, but recognition keeps startup tools until restart.
    let inspected = inspection_tools(app);
    let mut engines = catalog::discover(&inspected, &scope).await;
    let native = if let Some(exe) = &inspected.speech {
        inspected
            .probe_json(exe, &["models".into(), "--json".into()], &scope)
            .await
            .unwrap_or(json!({}))
    } else {
        json!({})
    };
    crate::runtime_install::refresh_health(app, &engines, &native).await;
    let health = app.runtime_health.lock().unwrap().clone();
    let pending = app.runtime_pending_restart.lock().unwrap().clone();
    for engine in engines.as_array_mut().into_iter().flatten() {
        if engine["id"] == crate::integrated::ENGINE && !app.runtime.config.enable_integrated_model
        {
            engine["ready"] = false.into();
            engine["reason"] = crate::integrated::DISABLED_REASON.into();
            continue;
        }
        let runtime = match engine["id"].as_str() {
            Some("whisper") => "whisper",
            Some(crate::nemo::ENGINE) => "nemo",
            Some("openai" | "compatible") => "provider",
            _ => "speech",
        };
        if pending.contains(runtime) || pending.contains("ffmpeg") {
            engine["ready"] = false.into();
            engine["reason"] = "実行環境の反映にはサーバーの再起動が必要です。".into();
        } else if health["ffmpeg"] != true {
            engine["ready"] = false.into();
            engine["reason"] =
                "FFmpeg/FFprobeの起動点検に失敗しました。管理画面で状態を確認してください。".into();
        }
    }
    let mut rows = vec![];
    for (engine, label, list) in
        std::iter::once(("whisper", "Whisper", catalog::WHISPER_MODELS.as_slice()))
            .chain(catalog::NATIVE.iter().map(|r| (r.0, r.1, r.2)))
            .chain(std::iter::once((
                crate::nemo::ENGINE,
                "Nemotron ASR",
                crate::nemo::MODELS.as_slice(),
            )))
            .chain(std::iter::once((
                "auxiliary",
                "補助モデル",
                [
                    ("silero-vad", "Silero VAD"),
                    ("nemotron-3-diarization", "Nemotron 3 Diarization"),
                ]
                .as_slice(),
            )))
    {
        let e = engines
            .as_array()
            .and_then(|r| r.iter().find(|e| e["id"] == engine));
        for (model, name) in list {
            let found = e
                .and_then(|e| e["models"].as_array())
                .and_then(|r| r.iter().find(|m| m["id"] == *model));
            let mut availability = found
                .and_then(|m| m["availability"].as_str())
                .unwrap_or("unavailable")
                .to_string();
            if engine == "auxiliary"
                && let Some(row) = native["models"]
                    .as_array()
                    .and_then(|r| r.iter().find(|m| m["name"] == *model))
            {
                let files = row["files"].as_array().cloned().unwrap_or_default();
                let files = files
                    .iter()
                    .filter(|f| f["type"] == row["type"])
                    .collect::<Vec<_>>();
                availability = if !files.is_empty() && files.iter().all(|f| f["fetched"] == true) {
                    "installed"
                } else {
                    "download_on_first_use"
                }
                .into();
            }
            let on_demand = engine == "auxiliary" || engine == crate::nemo::ENGINE;
            let installable = if engine == "whisper" {
                app.tools().whisper.is_some()
            } else if engine == crate::nemo::ENGINE {
                app.tools().nemo.is_some() && availability != "unavailable"
            } else {
                app.tools().speech.is_some() && availability != "unavailable"
            };
            let policy = if on_demand { "on_demand" } else { "resident" };
            let methods = e
                .map(|e| e["processingMethods"].clone())
                .unwrap_or(json!([]));
            rows.push(json!({"id":format!("{engine}/{model}"),"engine":engine,
                "engineLabel":label,"label":name,"availability":availability,
                "installable":installable,"loadable":!on_demand&&availability=="installed",
                "loadPolicy":policy,"processingMethods":methods}));
            if engine == crate::integrated::ENGINE {
                let row = rows.last_mut().unwrap();
                row["downloadBytes"] = crate::integrated::MODEL_BYTES.into();
                row["manualInstallRequired"] = true.into();
            }
            let runtime_id = if engine == "whisper" {
                "whisper"
            } else if engine == crate::nemo::ENGINE {
                "nemo"
            } else {
                "speech"
            };
            let runtime_present = match runtime_id {
                "whisper" => inspected.whisper.is_some(),
                "nemo" => inspected.nemo.is_some(),
                _ => inspected.speech.is_some(),
            };
            let supported = if engine == "auxiliary" {
                native["models"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|r| r["name"] == *model))
            } else {
                found.is_some() && availability != "unavailable"
            };
            let healthy = health[runtime_id] == true;
            let staged = crate::runtime_install::package(runtime_id).is_ok_and(|p| {
                p.programs.iter().all(|r| {
                    crate::runtime_install::managed_program(&app.runtime.home, runtime_id, r.0)
                        .is_some()
                })
            });
            let media = health["ffmpeg"] == true && !pending.contains("ffmpeg");
            let state = if pending.contains(runtime_id) || pending.contains("ffmpeg") {
                "restart_required"
            } else {
                management_state(
                    runtime_present,
                    staged,
                    healthy,
                    supported,
                    media,
                    &availability,
                )
            };
            let mut missing = vec![];
            if !runtime_present || !healthy {
                missing.push(runtime_id);
            }
            if !media {
                missing.push("ffmpeg");
            }
            let row = rows.last_mut().unwrap();
            row["runtimeId"] = runtime_id.into();
            row["missingRuntimes"] = json!(missing);
            row["managementState"] = state.into();
            let managed = matches!(runtime_id, "whisper" | "speech" | "nemo");
            let can_support = (supported && healthy) || (managed && (!runtime_present || !healthy));
            row["supported"] = can_support.into();
            row["supportStatus"] = if supported && healthy {
                "confirmed"
            } else if !runtime_present && managed {
                "managed_runtime"
            } else if healthy {
                "unsupported"
            } else {
                "unknown"
            }
            .into();
            row["runtimeInstallable"] = (managed && !runtime_present).into();
            row["runtimeReady"] =
                (runtime_present && healthy && supported && !pending.contains(runtime_id)).into();
            row["modelInstalled"] = (availability == "installed").into();
            row["available"] = (runtime_present
                && healthy
                && supported
                && media
                && availability == "installed"
                && !pending.contains(runtime_id))
            .into();
            row["installable"] = (row["installable"] == true
                && healthy
                && supported
                && !pending.contains(runtime_id))
            .into();
            row["loadable"] = (row["loadable"] == true
                && healthy
                && supported
                && media
                && !pending.contains(runtime_id))
            .into();
            row["statusMessage"] = match state {
                "missing_runtime" if runtime_id == "whisper" => {
                    "Whisper専用実行環境を導入してください。PCのPythonは変更しません。"
                }
                "runtime_error" => {
                    "実行ファイルの点検に失敗しました。依存DLL・破損・実行権限を確認してください。"
                }
                "unsupported" => "導入済みの実行環境がこのモデルに対応していません。",
                "restart_required" => "反映にはサーバーの再起動が必要です。PCの再起動は不要です。",
                _ => "",
            }
            .into();
            if engine == crate::integrated::ENGINE && !app.runtime.config.enable_integrated_model {
                row["managementState"] = "disabled_by_policy".into();
                row["statusMessage"] = crate::integrated::DISABLED_REASON.into();
                row["installable"] = false.into();
                row["loadable"] = false.into();
                row["available"] = false.into();
                row["missingRuntimes"] = json!([]);
                row["runtimeInstallable"] = false.into();
            }
        }
    }
    let runtimes = crate::runtime_install::inventory(app);
    for row in &mut rows {
        let allowed = row["managementState"] != "disabled_by_policy";
        row["installable"] = (allowed && install_plan(row, &runtimes).is_ok()).into();
        row["deletable"] = (allowed
            && row["modelInstalled"] == true
            && (row["engine"] == "whisper"
                || row["engine"] == crate::nemo::ENGINE
                || health["speech"] == true))
            .into();
    }
    *app.engines.lock().unwrap() = engines;
    *app.model_inventory.lock().unwrap() = json!(rows);
}
fn inspection_tools(app: &App) -> Tools {
    let tools = Tools::resolve(&app.runtime);
    let mut pending = app.runtime_pending_restart.lock().unwrap();
    for (id, changed) in [
        (
            "whisper",
            tools.whisper.is_some() && tools.whisper != app.tools().whisper,
        ),
        (
            "speech",
            tools.speech.is_some() && tools.speech != app.tools().speech,
        ),
        (
            "nemo",
            tools.nemo.is_some() && tools.nemo != app.tools().nemo,
        ),
        (
            "ffmpeg",
            tools.ffmpeg.is_some()
                && tools.ffprobe.is_some()
                && (tools.ffmpeg != app.tools().ffmpeg || tools.ffprobe != app.tools().ffprobe),
        ),
    ] {
        if changed {
            pending.insert(id.into());
        }
    }
    tools
}
fn model_label(id: &str) -> String {
    if let Ok((engine, model)) = selection(id) {
        let entries: Vec<(&str, &str)> = match engine {
            "whisper" => catalog::WHISPER_MODELS.to_vec(),
            crate::nemo::ENGINE => crate::nemo::MODELS.to_vec(),
            "auxiliary" => vec![
                ("silero-vad", "Silero VAD"),
                ("nemotron-3-diarization", "Nemotron 3 Diarization"),
            ],
            _ => catalog::NATIVE
                .iter()
                .find(|r| r.0 == engine)
                .map(|r| r.2.to_vec())
                .unwrap_or_default(),
        };
        if let Some(row) = entries.iter().find(|r| r.0 == model) {
            return row.1.into();
        }
    }
    crate::runtime_install::package(id)
        .map(|p| p.label.to_string())
        .unwrap_or_else(|_| "モデル一覧".into())
}
/// Validate all prerequisites before downloading any of them; preserve configured external tools.
fn install_plan(model: &Value, runtimes: &Value) -> Result<Vec<String>> {
    if matches!(
        model["managementState"].as_str(),
        Some("disabled_by_policy" | "unsupported" | "preview_unknown")
    ) {
        bail!("このモデルは現在の実行環境へ導入できません。")
    }
    let mut plan = vec![];
    for id in model["missingRuntimes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let runtime = runtimes
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == id))
            .context("実行環境の状態を再点検してください")?;
        if matches!(
            runtime["state"].as_str(),
            Some("detected" | "restart_required")
        ) {
            continue;
        }
        if runtime["installable"] != true {
            bail!(
                "{}は自動導入・修復できません。設定された配置を確認してください。",
                model_label(id)
            );
        }
        if !plan.iter().any(|v| v == id) {
            plan.push(id.to_string());
        }
    }
    Ok(plan)
}
fn operation_target(app: &App, label: &str) {
    let mut operation = app.management.lock().unwrap();
    operation["targetLabel"] = label.into();
    for key in [
        "receivedBytes",
        "downloadedBytes",
        "totalBytes",
        "phase",
        "progressPercent",
        "progressLabel",
        "progressUnit",
        "progressCurrent",
        "progressTotal",
        "gpuFallback",
    ] {
        operation.as_object_mut().unwrap().remove(key);
    }
}
fn completed_message(action: &str, label: &str, restart: bool, fallback: bool) -> String {
    let result = match action {
        "refresh" => "モデル一覧の再点検が完了しました。".into(),
        "install" | "install-runtime" => format!("{label}の導入が完了しました。"),
        "delete" => format!("{label}の保存ファイルを削除しました。"),
        "unload" => format!("{label}を選択した処理枠のメモリから解放しました。"),
        "load" if fallback => {
            format!("{label}をCPUでロードしました（GPUで実行できなかったため切り替えました）。")
        }
        "load" => format!("{label}をロードしました。"),
        _ => format!("{label}の操作が完了しました。"),
    };
    if restart && matches!(action, "install" | "install-runtime") {
        format!(
            "{result} 導入した実行環境の反映にはサーバーの再起動が必要です。PCの再起動は不要です。"
        )
    } else {
        result
    }
}
fn management_state(
    present: bool,
    staged: bool,
    healthy: bool,
    supported: bool,
    media: bool,
    availability: &str,
) -> &'static str {
    if !present {
        if staged {
            "restart_required"
        } else {
            "missing_runtime"
        }
    } else if !healthy {
        "runtime_error"
    } else if !supported {
        "unsupported"
    } else if !media {
        "missing_media"
    } else if availability == "installed" {
        "installed"
    } else {
        "model_missing"
    }
}
pub async fn status(app: &App) -> Result<Value> {
    let scheduling = crate::scheduler::status(app).await;
    let mut loaded = vec![];
    for lane in 0..2 {
        if let Ok(mut slot) = app.slots[lane].try_lock()
            && {
                if slot.as_mut().is_some_and(|s| !s.alive()) {
                    *slot = None;
                }
                true
            }
            && let Some(s) = slot.as_ref()
        {
            loaded.push(json!({"id":format!("whisper/{}",s.model),"lane":lane,"device":s.device,"pinned":s.pinned,"requests":s.requests}));
        }
        if let Ok(mut slot) = app.native_slots[lane].try_lock()
            && {
                if slot
                    .as_mut()
                    .is_some_and(|s| !matches!(s.child.try_wait(), Ok(None)))
                {
                    *slot = None;
                }
                true
            }
            && let Some(s) = slot.as_ref()
        {
            loaded.push(json!({"id":s.id,"lane":lane,"device":s.device,"pinned":true,"requests":s.requests}));
        }
    }
    let mut models = json!(
        app.model_inventory
            .lock()
            .unwrap()
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
    );
    let operation = app.management.lock().unwrap().clone();
    let runtime_status = app.status().await?;
    let engines = app.engines.lock().unwrap().clone();
    let mut runtimes = crate::runtime_install::inventory(app);
    let applying = app.preparing()
        || (app.maintenance.load(Ordering::Acquire)
            && matches!(
                operation["action"].as_str(),
                Some("install" | "install-runtime" | "install-all")
            ));
    if applying {
        for row in runtimes.as_array_mut().into_iter().flatten() {
            if row["restartRequired"] == true {
                row["restartRequired"] = false.into();
                row["state"] = "activating".into();
            }
        }
    }
    let restart_required = runtimes
        .as_array()
        .is_some_and(|r| r.iter().any(|r| r["restartRequired"] == true));
    for model in models.as_array_mut().into_iter().flatten() {
        model["restartRequired"] = (!applying
            && model["modelInstalled"] == true
            && model["managementState"] == "restart_required")
            .into();
    }
    let restart_blocked = app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.update_busy.load(Ordering::Acquire)
        || app.incoming.load(Ordering::Acquire) > 0
        || !app.running.lock().unwrap().is_empty()
        || !app.scheduler.lock().unwrap().active.is_empty()
        || !app.store.active_jobs()?.is_empty();
    let rows = engines.as_array();
    let gpu = rows
        .and_then(|r| r.iter().find(|e| e["id"] == "whisper"))
        .map(|e| json!({"computeType":e["gpuComputeType"],"note":e["gpuNote"]}));
    let external_ready = rows.is_some_and(|r| {
        r.iter()
            .any(|e| e["externalApi"] == true && e["ready"] == true)
    });
    let networks = app
        .policy
        .networks
        .iter()
        .map(|n| n.cidr.clone())
        .collect::<Vec<_>>();
    let network = json!({"urls":app.policy.urls(&app.runtime.config.host),
        "allowedNetworks":networks,"authMode":app.health()["authMode"]});
    let tools = json!({"whisper":app.tools().whisper.is_some(),"speech":app.tools().speech.is_some(),
        "nemo":app.tools().nemo.is_some(),"ffmpeg":app.tools().ffmpeg.is_some(),
        "ffprobe":app.tools().ffprobe.is_some()});
    let migration = crate::config::read_json(&app.store.dir.join("migration.json"))
        .ok()
        .map(|_| json!({"completed":true,"legacyRetained":true}));
    let runtime_storage = crate::runtime_storage::snapshot(app).await;
    Ok(
        json!({"version":crate::config::VERSION,"models":models,"runtimes":runtimes,"recommendedModels":RECOMMENDED_MODELS,
        "restartRequired":restart_required,"restartBlocked":restart_blocked,"loaded":loaded,
        "operation":operation,"busy":app.maintenance.load(Ordering::Acquire),
        "modelResults":app.management_results.lock().unwrap().clone(),
        "runtimeSetup":app.runtime_setup.lock().unwrap().clone(),"lifecycle":crate::lifecycle::status(app),
        "runtimeStorage":runtime_storage,
        "status":runtime_status,"gpu":gpu,"network":network,"scheduling":scheduling,
        "runtimeHealth":app.runtime_health.lock().unwrap().clone(),
        "externalApiReady":external_ready,"tools":tools,
        "dataDirectory":app.store.dir.to_string_lossy(),
        "dataFolderName":app.store.dir.file_name().and_then(|v|v.to_str()),
        "migration":migration}),
    )
}
/// Freeze only eligible catalog IDs. Never include display-only/policy-disabled models.
fn bulk_targets(
    models: &Value,
    runtimes: &Value,
    loaded: &Value,
    action: &str,
) -> (Vec<String>, Vec<Value>) {
    let mut targets = vec![];
    let mut skipped = vec![];
    for model in models.as_array().into_iter().flatten() {
        let id = model["id"].as_str().unwrap_or("");
        if action == "install-recommended" && !RECOMMENDED_MODELS.contains(&id) {
            continue;
        }
        let installed = model["modelInstalled"] == true || model["availability"] == "installed";
        let reason = if model["displayOnly"] == true
            || model["managementState"] == "disabled_by_policy"
            || selection(id).is_err()
        {
            "導入・削除の対象外"
        } else if matches!(action, "install-all" | "install-recommended") {
            if model["installable"] != true {
                "この実行環境では自動導入できません"
            } else if installed && install_plan(model, runtimes).is_ok_and(|plan| plan.is_empty()) {
                "導入済み"
            } else {
                ""
            }
        } else if !installed {
            "未導入"
        } else if loaded
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == id))
        {
            "ロード済みのため削除対象から除外"
        } else if model["deletable"] != true {
            "現在の実行環境では削除できません"
        } else {
            ""
        };
        if reason.is_empty() {
            targets.push(id.to_string());
        } else {
            skipped.push(json!({"model":id,"reason":reason}));
        }
    }
    (targets, skipped)
}
struct Fence(Arc<App>);
impl Drop for Fence {
    fn drop(&mut self) {
        self.0.maintenance.store(false, Ordering::Release);
    }
}
pub async fn action(app: Arc<App>, body: Value) -> Result<Value> {
    let action = body["action"]
        .as_str()
        .context("操作を指定してください")?
        .to_string();
    if action == "cancel" {
        app.management_cancel.lock().unwrap().cancel();
        return Ok(json!({"accepted":true}));
    }
    if !matches!(
        action.as_str(),
        "refresh"
            | "install"
            | "load"
            | "unload"
            | "install-runtime"
            | "delete"
            | "install-all"
            | "install-recommended"
            | "delete-all"
    ) {
        bail!("未対応の管理操作です")
    }
    let id = if matches!(
        action.as_str(),
        "refresh" | "install-all" | "install-recommended" | "delete-all"
    ) {
        String::new()
    } else {
        body[if action == "install-runtime" {
            "runtime"
        } else {
            "model"
        }]
        .as_str()
        .unwrap_or("")
        .to_string()
    };
    let bulk = matches!(
        action.as_str(),
        "install-all" | "install-recommended" | "delete-all"
    );
    if action == "install-runtime" {
        crate::runtime_install::package(&id)?;
    } else if action != "refresh" && !bulk {
        let (engine, _) = selection(&id)?;
        if engine == crate::integrated::ENGINE && action != "unload" {
            crate::integrated::check_enabled(&app)?;
        }
    }
    if action == "delete" && body["confirm"] != "delete-model" {
        bail!("モデル削除の確認が必要です")
    }
    if action == "delete-all" && body["confirm"] != "delete-all-models" {
        bail!("モデル一括削除の確認が必要です")
    }
    let lane = if let Some(value) = body.get("lane") {
        value.as_u64().context("処理枠が不正です")?
    } else {
        0
    } as usize;
    if lane > 1 {
        bail!("処理枠が不正です")
    }
    let device = body["device"].as_str().unwrap_or("cpu").to_string();
    if !matches!(device.as_str(), "cpu" | "auto" | "gpu") {
        bail!("処理方法が不正です")
    }
    let _gate = app.admission.lock().await;
    if app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.update_busy.load(Ordering::Acquire)
        || app.incoming.load(Ordering::Acquire) > 0
        || !app.scheduler.lock().unwrap().active.is_empty()
        || !app.store.active_jobs()?.is_empty()
    {
        bail!("CONFLICT: 処理・転送・更新・モデル操作中です。完了後に操作してください。")
    }
    let (mut targets, skipped) = if bulk {
        let current = status(&app).await?;
        bulk_targets(
            &current["models"],
            &current["runtimes"],
            &current["loaded"],
            &action,
        )
    } else {
        (vec![id.clone()], vec![])
    };
    // A deletion confirmation applies only to the exact IDs the user saw.
    if action == "delete-all" {
        let requested = body["models"]
            .as_array()
            .context("削除対象一覧を指定してください")?;
        let requested = requested
            .iter()
            .map(|v| v.as_str().context("削除対象IDが不正です"))
            .collect::<Result<Vec<_>>>()?;
        if requested.is_empty()
            || requested
                .iter()
                .any(|id| !targets.iter().any(|allowed| allowed == id))
        {
            bail!("CONFLICT: 削除対象が変わりました。モデル一覧を再点検してください。")
        }
        targets.retain(|id| requested.contains(&id.as_str()));
    }
    if targets.is_empty() {
        bail!("操作が必要な対象モデルはありません。")
    }
    app.maintenance.store(true, Ordering::Release);
    let cancel = CancellationToken::new();
    *app.management_cancel.lock().unwrap() = cancel.clone();
    let operation = uuid::Uuid::new_v4().simple().to_string();
    let accepted_id = operation.clone();
    for target in &targets {
        app.management_results
            .lock()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(target);
    }
    *app.management.lock().unwrap() = json!({"id":operation,"state":"running","action":action,
        "model":targets[0],"targets":targets,"skipped":skipped,"totalCount":targets.len(),"completedCount":0,
        "message":"モデル操作を実行しています。"});
    let task = app.clone();
    tokio::spawn(async move {
        let _fence = Fence(task.clone());
        let item_action = match action.as_str() {
            "install-all" | "install-recommended" => "install",
            "delete-all" => "delete",
            other => other,
        };
        let mut failed = false;
        let mut completed = 0;
        for target in &targets {
            if cancel.is_cancelled() || task.stop.is_cancelled() {
                break;
            }
            {
                let mut current = task.management.lock().unwrap();
                current["model"] = target.clone().into();
            }
            operation_target(&task, &model_label(target));
            let work = tokio::time::timeout(
                Duration::from_secs(3600),
                operate(&task, item_action, target, lane, &device, &cancel),
            );
            let mut result = tokio::select! {
                _ = task.stop.cancelled() => Err(anyhow::anyhow!("サーバーの終了により中止しました")),
                _ = cancel.cancelled() => Err(anyhow::anyhow!("モデル操作を中止しました")),
                result = work => result.context("モデル操作がタイムアウトしました").and_then(|v| v),
            };
            if matches!(item_action, "install" | "install-runtime" | "delete")
                && !task.stop.is_cancelled()
            {
                let pending_runtime = !task.runtime_pending_restart.lock().unwrap().is_empty();
                if pending_runtime
                    && let Err(error) = crate::runtime_bootstrap::activate(&task).await
                {
                    result = Err(error);
                }
                refresh(&task).await;
            }
            let (state, message) = match result {
                Ok(()) => (
                    "complete",
                    completed_message(
                        item_action,
                        &model_label(target),
                        !task.runtime_pending_restart.lock().unwrap().is_empty(),
                        task.management.lock().unwrap()["gpuFallback"] == true,
                    ),
                ),
                Err(error) => (
                    if cancel.is_cancelled() || task.stop.is_cancelled() {
                        "cancelled"
                    } else {
                        "error"
                    },
                    format!("{}：{}", model_label(target), error)
                        .chars()
                        .take(2000)
                        .collect::<String>(),
                ),
            };
            failed |= state == "error";
            task.management_results.lock().unwrap()[target] =
                json!({"state":state,"action":item_action,"message":message});
            completed += 1;
            {
                let mut finished = task.management.lock().unwrap();
                finished["completedCount"] = completed.into();
                finished["message"] = message.into();
            }
        }
        let cancelled = cancel.is_cancelled() || task.stop.is_cancelled();
        if cancelled {
            for target in targets.iter().skip(completed) {
                task.management_results.lock().unwrap()[target] = json!({"state":"cancelled","action":item_action,"message":"処理前に中止しました。"});
            }
        }
        let mut finished = task.management.lock().unwrap();
        finished["id"] = operation.into();
        finished["state"] = if cancelled {
            "cancelled"
        } else if failed {
            "error"
        } else {
            "complete"
        }
        .into();
        if bulk {
            finished["message"] = format!(
                "一括操作: {completed}/{}件処理{}",
                targets.len(),
                if cancelled {
                    "・中止"
                } else if failed {
                    "・エラーあり（各行を確認）"
                } else {
                    "・完了"
                }
            )
            .into();
        }
    });
    Ok(json!({"accepted":true,"id":accepted_id}))
}
pub(crate) async fn operate(
    app: &App,
    action: &str,
    id: &str,
    lane: usize,
    device: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    if action == "install-runtime" {
        operation_target(app, &model_label(id));
        crate::runtime_install::install(app, id, cancel).await?;
        return Ok(());
    }
    if action == "refresh" {
        refresh(app).await;
        return Ok(());
    }
    let (engine, model) = selection(id)?;
    if engine == crate::integrated::ENGINE && action != "unload" {
        crate::integrated::check_enabled(app)?;
    }
    if action == "delete" {
        for index in 0..crate::scheduler::WORKERS {
            if app.slots[index]
                .lock()
                .await
                .as_ref()
                .is_some_and(|s| engine == "whisper" && s.model == model)
                || app.native_slots[index]
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|s| s.id == id)
            {
                bail!("ロード済みです。両方の処理枠からアンロードしてから削除してください。")
            }
        }
        operation_target(app, &model_label(id));
        let tools = inspection_tools(app);
        if engine == "whisper" {
            crate::model_files::delete_whisper(&tools, model)?;
        } else if engine == crate::nemo::ENGINE {
            crate::model_files::delete_nemo(&tools)?;
        } else {
            let exe = tools
                .speech
                .as_deref()
                .context("モデル削除にはspeech.cppが必要です")?;
            run_with(app, &tools, exe, &["rm".into(), model.into()], cancel).await?;
        }
        return Ok(());
    }
    if action == "unload" {
        let mut removed = false;
        for index in [lane] {
            let mut whisper = app.slots[index].lock().await;
            if whisper
                .as_ref()
                .is_some_and(|s| engine == "whisper" && s.model == model)
            {
                whisper.as_mut().unwrap().unload().await?;
                *whisper = None;
                removed = true
            }
            let mut native = app.native_slots[index].lock().await;
            if native.as_ref().is_some_and(|s| s.id == id) {
                let s = native.as_mut().unwrap();
                s._scope.terminate()?;
                tokio::time::timeout(Duration::from_secs(15), s.child.wait())
                    .await
                    .context("ネイティブモデルの終了未確認")??;
                *native = None;
                removed = true
            }
        }
        if !removed {
            bail!("このモデルは選択した処理枠にはロードされていません。")
        };
        return Ok(());
    }
    if action == "install" {
        refresh(app).await;
        let inventory = app.model_inventory.lock().unwrap().clone();
        let entry = inventory
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == id))
            .context("モデル一覧を再点検してください")?;
        let plan = install_plan(entry, &crate::runtime_install::inventory(app))?;
        for runtime in plan {
            operation_target(
                app,
                &format!("{} → {}", model_label(id), model_label(&runtime)),
            );
            crate::runtime_install::install(app, &runtime, cancel).await?;
        }
        operation_target(app, &model_label(id));
        let tools = inspection_tools(app);
        if engine == crate::nemo::ENGINE {
            crate::nemo::install(app, cancel).await?;
        } else if engine == "whisper" {
            let (exe, args) = tools
                .whisper
                .as_ref()
                .context("Whisperワーカーがありません")?;
            let mut args = args.clone();
            args.extend(["--download".into(), model.into()]);
            run_with(app, &tools, exe, &args, cancel).await?;
        } else {
            let exe = tools
                .speech
                .as_ref()
                .context("speech.cppをセットアップしてください")?;
            run_with(app, &tools, exe, &["pull".into(), model.into()], cancel).await?;
            if engine != "auxiliary" && engine != crate::integrated::ENGINE {
                operation_target(app, &format!("{} → Silero VAD", model_label(id)));
                run_with(
                    app,
                    &tools,
                    exe,
                    &["pull".into(), "silero-vad".into()],
                    cancel,
                )
                .await?;
            }
        }
        return Ok(());
    }
    let inventory = app.model_inventory.lock().unwrap().clone();
    let entry = inventory
        .as_array()
        .and_then(|r| r.iter().find(|r| r["id"] == id))
        .context("モデルを点検してください")?;
    if entry["loadable"] != true {
        bail!("先にモデルをインストールしてください。補助モデルは認識時にロード・解放されます。")
    }
    let gpu = entry["processingMethods"]
        .as_array()
        .is_some_and(|r| r.iter().any(|m| m["id"] == "gpu"));
    if device == "gpu" && !gpu {
        bail!("GPUの実行候補がありません。CUDA/Vulkan環境を点検してください。")
    }
    let requested_auto = device == "auto";
    let unknown_capacity =
        requested_auto && gpu && !app.scheduler.lock().unwrap().gpu_capacity_known();
    let device = if requested_auto {
        if gpu && !unknown_capacity {
            "gpu"
        } else {
            "cpu"
        }
    } else {
        device
    };
    if unknown_capacity {
        let mut state = app.management.lock().unwrap();
        state["gpuFallback"] = true.into();
        state["gpuFallbackBeforeExecution"] = true.into();
        state["gpuFallbackReasonCode"] = "gpu_capacity_unknown".into();
        state["gpuFallbackReason"] =
            "GPU容量を確認できずVRAM上限も未設定のためCPUを選択しました。".into();
    }
    crate::scheduler::check_manual(app, lane, engine, model, device).await?;
    crate::scheduler::cleanup(app, lane, false).await?;
    let result = load_selected(app, lane, engine, id, model, device, cancel).await;
    match result {
        Err(gpu_error)
            if requested_auto
                && device == "gpu"
                && !cancel.is_cancelled()
                && !app.stop.is_cancelled() =>
        {
            crate::scheduler::cleanup(app, lane, false).await?;
            load_selected(app, lane, engine, id, model, "cpu", cancel)
                .await
                .map_err(|cpu_error| {
                    anyhow::anyhow!("GPUロード: {gpu_error}; CPUロード: {cpu_error}")
                })?;
            let mut state = app.management.lock().unwrap();
            state["gpuFallback"] = true.into();
            state["gpuFallbackReason"] = gpu_error
                .to_string()
                .chars()
                .take(2000)
                .collect::<String>()
                .into();
            Ok(())
        }
        result => result,
    }
}
#[allow(clippy::too_many_arguments)]
async fn load_selected(
    app: &App,
    lane: usize,
    engine: &str,
    id: &str,
    model: &str,
    device: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    if engine == "whisper" {
        crate::jobs::preload(app, lane, model, device, cancel).await
    } else {
        let path = if engine == crate::integrated::ENGINE {
            Some(crate::integrated::require_model(&app.tools(), &app.supervisor).await?)
        } else {
            None
        };
        let session = NativeSession::load(
            app,
            id,
            path.as_ref().and_then(|p| p.to_str()).unwrap_or(model),
            device,
            cancel,
            None,
        )
        .await?;
        *app.native_slots[lane].lock().await = Some(session);
        Ok(())
    }
}
async fn run_with(
    app: &App,
    tools: &Tools,
    exe: &Path,
    args: &[String],
    cancel: &CancellationToken,
) -> Result<()> {
    let scope = platform::Job::new()?;
    let mut cmd = tools.command(exe);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SPEECH_NO_UPDATE_CHECK", "1");
    for name in [
        "MEETING_SERVER_API_KEY",
        "TRANSCRIPTION_GITHUB_TOKEN",
        "OPENAI_API_KEY",
        "TRANSCRIPTION_API_KEY",
    ] {
        cmd.env_remove(name);
    }
    let mut child = cmd.spawn()?;
    app.supervisor.assign(&child)?;
    scope.assign(&child)?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let (_, errors) = tokio::select! {
        _ = cancel.cancelled() => bail!("モデル操作を中止しました"),
        result = async { tokio::try_join!(drain_progress(app, out, false), drain_progress(app, err, true)) } => result?,
    };
    if !child.wait().await?.success() {
        bail!(
            "モデル操作に失敗しました: {}",
            String::from_utf8_lossy(&errors)
                .chars()
                .take(1200)
                .collect::<String>()
        )
    }
    Ok(())
}
async fn drain_progress(
    app: &App,
    mut reader: impl tokio::io::AsyncRead + Unpin,
    capture: bool,
) -> std::io::Result<Vec<u8>> {
    let mut errors = Vec::new();
    let mut pending = Vec::new();
    let mut block = [0; 4096];
    loop {
        let count = reader.read(&mut block).await?;
        if count == 0 {
            break;
        }
        if capture && errors.len() < 8192 {
            errors.extend_from_slice(&block[..count.min(8192 - errors.len())]);
        }
        for byte in &block[..count] {
            if matches!(byte, b'\r' | b'\n') {
                report_progress(app, &String::from_utf8_lossy(&pending));
                pending.clear();
            } else if pending.len() < 8192 {
                pending.push(*byte);
            }
        }
        report_progress(app, &String::from_utf8_lossy(&pending));
    }
    report_progress(app, &String::from_utf8_lossy(&pending));
    Ok(errors)
}
fn report_progress(app: &App, line: &str) {
    let report = serde_json::from_str::<Value>(line)
        .ok()
        .filter(|v| v["type"] == "download_progress");
    let percent = report
        .as_ref()
        .and_then(|v| v["percent"].as_f64())
        .or_else(|| {
            // speech.cpp uses a terminal progress bar on stderr. Read actual percentages only.
            let end = line.find('%')?;
            let start = line[..end]
                .rfind(|c: char| !c.is_ascii_digit() && c != '.')
                .map_or(0, |p| p + 1);
            line[start..end].parse::<f64>().ok()
        });
    let Some(percent) = percent.filter(|p| p.is_finite() && (0.0..=100.0).contains(p)) else {
        return;
    };
    let mut current = app.management.lock().unwrap();
    current["progressPercent"] = percent.into();
    current["phase"] = "download".into();
    if let Some(report) = report {
        current["progressLabel"] = report["label"]
            .as_str()
            .unwrap_or("取得")
            .chars()
            .take(120)
            .collect::<String>()
            .into();
        current["progressUnit"] = report["unit"].as_str().unwrap_or("files").into();
        current["progressCurrent"] = report["current"].clone();
        current["progressTotal"] = report["total"].clone();
    }
}
pub struct NativeSession {
    pub id: String,
    pub device: String,
    pub requests: u64,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    _scope: Arc<platform::Job>,
}
impl NativeSession {
    pub async fn unload(&mut self) -> Result<()> {
        self._scope.terminate()?;
        tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .context("ネイティブモデルの終了未確認")??;
        crate::scheduler::wait_scope(&self._scope).await
    }
    pub fn scope(&self) -> Arc<platform::Job> {
        self._scope.clone()
    }
    pub(crate) async fn load(
        app: &App,
        id: &str,
        model: &str,
        device: &str,
        cancel: &CancellationToken,
        job_id: Option<&str>,
    ) -> Result<Self> {
        let tools = app.tools();
        let exe = tools.speech.as_ref().context("speech.cppがありません")?;
        let tools = app.tools();
        let mut cmd = tools.command(exe);
        cmd.args(["worker", model, "--device", device])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("SPEECH_NO_UPDATE_CHECK", "1");
        cmd.args(["--threads", &crate::jobs::whisper_threads(app).to_string()]);
        let mut child = cmd.spawn()?;
        let scope = if let Some(job_id) = job_id {
            app.job_scope(job_id)?
        } else {
            Arc::new(platform::Job::new()?)
        };
        app.supervisor.assign(&child)?;
        scope.assign(&child)?;
        let mut session = Self {
            id: id.into(),
            device: device.into(),
            requests: 0,
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            _scope: scope,
        };
        let ready = session.read(cancel).await?;
        if ready["type"] != "ready" || ready["protocol"] != 4 {
            bail!(
                "ネイティブモデルのロードに失敗しました: {}",
                ready["error"]["message"]
                    .as_str()
                    .unwrap_or("未対応のワーカープロトコル")
            )
        }
        Ok(session)
    }
    async fn read(&mut self, cancel: &CancellationToken) -> Result<Value> {
        let mut line = String::new();
        let n = tokio::select! {
            _ = cancel.cancelled() => bail!("モデル操作を中止しました"),
            v = self.output.read_line(&mut line) => v?,
        };
        let limit = if self.id.starts_with("nemotron-transcribe/") {
            8
        } else {
            2
        } * 1024
            * 1024;
        if n == 0 || line.len() > limit {
            bail!("ネイティブワーカーの応答が不正です")
        };
        Ok(serde_json::from_str(&line)?)
    }
    pub async fn transcribe(
        &mut self,
        pcm: &[u8],
        options: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        self.request(pcm, options, cancel).await
    }
    pub(crate) async fn transcribe_integrated(
        &mut self,
        pcm: &[u8],
        speakers: bool,
        mut options: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        options["timestamps"] = true.into();
        options["speakers"] = speakers.into();
        self.request(pcm, options, cancel).await
    }
    async fn request(
        &mut self,
        pcm: &[u8],
        options: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        use base64::Engine;
        if self.child.try_wait()?.is_some() {
            bail!("ロード済みモデルが終了しています")
        };
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut command = json!({"type":"transcribe","id":id,"sample_rate":16000});
        command
            .as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        for (seq, pcm) in pcm.chunks(64_000).enumerate() {
            let chunk = json!({"type":"chunk","id":id,"seq":seq,"pcm":base64::engine::general_purpose::STANDARD.encode(pcm)});
            let line = format!("{chunk}\n");
            tokio::select! {
                _ = cancel.cancelled() => bail!("モデル操作を中止しました"),
                result = self.input.write_all(line.as_bytes()) => result?,
            };
        }
        for value in [command] {
            self.input
                .write_all(format!("{value}\n").as_bytes())
                .await?;
        }
        self.input.flush().await?;
        loop {
            let message = self.read(cancel).await?;
            if message["id"] != id {
                bail!("ワーカー応答の要求IDが一致しません")
            }
            match message["type"].as_str() {
                Some("progress") => (),
                Some("end") => {
                    self.requests += 1;
                    return Ok(message);
                }
                _ => bail!(
                    "ネイティブ認識エラー: {}",
                    message["error"]["message"]
                        .as_str()
                        .unwrap_or("認識が中断しました")
                ),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn recommendations_include_only_approved_accuracy_models_and_skip_installed() {
        use super::*;
        let models = json!([
            {"id":"haas/haas-ja-v1.0","installable":true},
            {"id":"qwen/qwen3-asr-1.7b","installable":true},
            {"id":"whisper/large-v3","installable":true},
            {"id":"whisper/small","installable":true},
            {"id":"qwen/qwen3-asr-0.6b","installable":true}
        ]);
        let (targets, _) = bulk_targets(&models, &json!([]), &json!([]), "install-recommended");
        assert_eq!(targets, RECOMMENDED_MODELS);
        let mut installed = models;
        installed[0]["modelInstalled"] = true.into();
        installed[0]["missingRuntimes"] = json!([]);
        let (targets, skipped) =
            bulk_targets(&installed, &json!([]), &json!([]), "install-recommended");
        assert_eq!(targets, ["qwen/qwen3-asr-1.7b"]);
        assert_eq!(skipped[0]["reason"], "導入済み");
    }
    #[test]
    fn bulk_selection_protects_policy_loaded_and_already_installed_models() {
        use super::*;
        let models = json!([
            {"id":"whisper/tiny","modelInstalled":true,"installable":true,"deletable":true,"missingRuntimes":[]},
            {"id":"whisper/base","modelInstalled":false,"installable":true},
            {"id":"nemotron-transcribe/nemotron-3.5-transcribe-30b-a4b","managementState":"disabled_by_policy","installable":true,"deletable":true,"modelInstalled":true},
            {"id":"unknown/model","installable":true},
            {"id":"auxiliary/silero-vad","modelInstalled":true,"deletable":true,"installable":false}
        ]);
        let (install, _) = bulk_targets(&models, &json!([]), &json!([]), "install-all");
        assert_eq!(install, vec!["whisper/base"]);
        let (delete, skipped) = bulk_targets(
            &models,
            &json!([]),
            &json!([{"id":"whisper/tiny","lane":1}]),
            "delete-all",
        );
        assert_eq!(delete, vec!["auxiliary/silero-vad"]);
        assert!(skipped.iter().any(|r| r["model"] == "whisper/tiny"
            && r["reason"].as_str().unwrap().contains("ロード済み")));
    }
    #[test]
    fn install_plan_reuses_ready_packages_and_validates_all_missing_dependencies() {
        use super::*;
        let model = json!({"managementState":"missing_runtime","missingRuntimes":["speech","ffmpeg","speech"]});
        let runtimes = json!([{"id":"speech","state":"missing","installable":true},{"id":"ffmpeg","state":"missing","installable":true}]);
        assert_eq!(
            install_plan(&model, &runtimes).unwrap(),
            vec!["speech", "ffmpeg"]
        );
        let ready =
            json!([{"id":"speech","state":"restart_required"},{"id":"ffmpeg","state":"detected"}]);
        assert!(install_plan(&model, &ready).unwrap().is_empty());
        let external = json!([{"id":"speech","state":"runtime_error","installable":false},{"id":"ffmpeg","state":"missing","installable":true}]);
        assert!(install_plan(&model, &external).is_err());
        assert!(install_plan(&json!({"managementState":"disabled_by_policy"}), &runtimes).is_err());
        assert_eq!(
            completed_message("install", "Whisper Base", false, false),
            "Whisper Baseの導入が完了しました。"
        );
        assert!(
            completed_message("install", "Nemotron ASR", true, false)
                .contains("サーバーの再起動が必要")
        );
        assert!(completed_message("delete", "Haas", false, false).contains("Haasの保存ファイル"));
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn staged_runtime_installs_weights_before_restart_and_deletion_protects_loaded_and_active_jobs()
     {
        use crate::config;
        use std::{fs, path::PathBuf};
        let root =
            std::env::temp_dir().join(format!("tc-model-management-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let exe = root.join("fixture.exe");
        let csc = PathBuf::from(std::env::var_os("WINDIR").unwrap())
            .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
        let output = platform::command(&csc)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([
                "/nologo",
                "/r:System.Web.Extensions.dll",
                &format!("/out:{}", exe.display()),
                "tests\\fixtures\\model-management-runtime.cs",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let runtime = |data: &str| config::Runtime {
            config: config::Config {
                instance_id: "model-management-fixture".into(),
                data_dir: root.join(data),
                ffmpeg: Some(exe.clone()),
                ffprobe: Some(exe.clone()),
                whisper_worker: Some(root.join("missing.exe")),
                ..Default::default()
            },
            root: root.clone(),
            home: root.join("settings"),
            installed: false,
            process_record: None,
            open: false,
        };
        let app = App::new(runtime("first-data")).await.unwrap();
        app.scheduler.lock().unwrap().test_memory = Some((131072, 131072));
        assert!(app.tools().speech.is_none());
        let package = crate::runtime_install::package("speech").unwrap();
        let staged = app.runtime.home.join("runtimes").join(format!(
            "speech-{}-{}",
            package.version,
            &package.sha256[..8]
        ));
        fs::create_dir_all(&staged).unwrap();
        fs::copy(&exe, staged.join("speech.exe")).unwrap();
        config::write_json(&staged.join("receipt.json"),&json!({"id":"speech","version":package.version,"archiveSha256":package.sha256,"programs":{"speech":"speech.exe"}})).unwrap();
        let cancel = CancellationToken::new();
        operate(&app, "install", "haas/haas-ja-v1.0", 0, "cpu", &cancel)
            .await
            .unwrap();
        refresh(&app).await;
        assert_eq!(
            fs::read_to_string(staged.join("operations.txt")).unwrap(),
            "pull haas-ja-v1.0\npull silero-vad\n"
        );
        assert!(app.tools().speech.is_none()); // Recognition cannot switch tools before restart.
        let rows = app.model_inventory.lock().unwrap().clone();
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "haas/haas-ja-v1.0")
            .unwrap();
        assert_eq!(row["modelInstalled"], true);
        assert_eq!(row["managementState"], "restart_required");
        assert_eq!(row["loadable"], false);
        assert!(
            status(&app).await.unwrap()["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == "haas/haas-ja-v1.0" && r["restartRequired"] == true)
        );
        app.stop.cancel();
        let restarted = App::new(runtime("second-data")).await.unwrap();
        restarted.scheduler.lock().unwrap().test_memory = Some((131072, 131072));
        let id = "haas/haas-ja-v1.0";
        assert!(
            action(restarted.clone(), json!({"action":"delete","model":id}))
                .await
                .is_err()
        );
        operate(&restarted, "load", id, 1, "cpu", &cancel)
            .await
            .unwrap();
        assert!(
            operate(&restarted, "delete", id, 0, "cpu", &cancel)
                .await
                .is_err()
        );
        assert!(
            operate(&restarted, "unload", id, 0, "cpu", &cancel)
                .await
                .is_err()
        );
        operate(&restarted, "unload", id, 1, "cpu", &cancel)
            .await
            .unwrap();
        let job = restarted
            .store
            .create(json!({"status":"queued","metadata":{}}))
            .unwrap();
        assert!(
            action(
                restarted.clone(),
                json!({"action":"delete","model":id,"confirm":"delete-model"})
            )
            .await
            .is_err()
        );
        restarted
            .store
            .update(job["id"].as_str().unwrap(), json!({"status":"complete"}))
            .unwrap();
        operate(&restarted, "delete", id, 0, "cpu", &cancel)
            .await
            .unwrap();
        refresh(&restarted).await;
        assert!(!staged.join("haas-ja-v1.0.weights").exists());
        assert!(staged.join("silero-vad.weights").exists());
        assert!(staged.join("speech.exe").exists());
        assert!(restarted.store.get(job["id"].as_str().unwrap()).is_ok());
        // Freeze a synthetic list: a failed model must not prevent subsequent valid targets.
        *restarted.model_inventory.lock().unwrap() = json!([
            {"id":"qwen/qwen3-asr-0.6b","installable":true,"modelInstalled":false},
            {"id":id,"installable":true,"modelInstalled":false}
        ]);
        action(restarted.clone(), json!({"action":"install-all"}))
            .await
            .unwrap();
        assert!(
            action(restarted.clone(), json!({"action":"refresh"}))
                .await
                .is_err()
        );
        for _ in 0..300 {
            if !restarted.maintenance.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!restarted.maintenance.load(Ordering::Acquire));
        assert_eq!(restarted.management.lock().unwrap()["totalCount"], 2);
        assert_eq!(restarted.management.lock().unwrap()["state"], "error");
        assert_eq!(
            restarted.management_results.lock().unwrap()[id]["state"],
            "complete"
        );
        assert!(staged.join("haas-ja-v1.0.weights").exists());
        assert!(action(restarted.clone(), json!({"action":"delete-all","confirm":"delete-all-models","models":["whisper/tiny"]})).await.is_err());
        action(restarted.clone(), json!({"action":"delete-all","confirm":"delete-all-models","models":[id,"auxiliary/silero-vad"]})).await.unwrap();
        for _ in 0..300 {
            if !restarted.maintenance.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!restarted.maintenance.load(Ordering::Acquire));
        assert_eq!(restarted.management.lock().unwrap()["state"], "complete");
        assert!(!staged.join("haas-ja-v1.0.weights").exists());
        assert!(!staged.join("silero-vad.weights").exists());
        assert!(staged.join("speech.exe").exists());
        assert!(restarted.store.get(job["id"].as_str().unwrap()).is_ok());
        fs::write(staged.join("slow-pull"), "fixture").unwrap();
        *restarted.model_inventory.lock().unwrap() = json!([
            {"id":id,"installable":true,"modelInstalled":false},
            {"id":"auxiliary/silero-vad","installable":true,"modelInstalled":false}
        ]);
        action(restarted.clone(), json!({"action":"install-all"}))
            .await
            .unwrap();
        for _ in 0..100 {
            if restarted.management.lock().unwrap()["progressPercent"] == 25 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            restarted.management.lock().unwrap()["progressPercent"],
            25.0
        );
        action(restarted.clone(), json!({"action":"cancel"}))
            .await
            .unwrap();
        for _ in 0..300 {
            if !restarted.maintenance.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!restarted.maintenance.load(Ordering::Acquire));
        assert_eq!(restarted.management.lock().unwrap()["state"], "cancelled");
        assert_eq!(
            restarted.management_results.lock().unwrap()["auxiliary/silero-vad"]["state"],
            "cancelled"
        );
        assert!(!staged.join("silero-vad.weights").exists());
        restarted.stop.cancel();
        drop(app);
        drop(restarted);
        // Worker tasks briefly retain SQLite handles until they observe cancellation.
        for _ in 0..100 {
            if fs::remove_dir_all(&root).is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fixture cleanup did not finish");
    }
    #[test]
    fn management_distinguishes_runtime_model_media_and_restart_states() {
        use super::management_state as state;
        assert_eq!(
            state(false, false, false, false, false, "unavailable"),
            "missing_runtime"
        );
        assert_eq!(
            state(false, true, false, false, false, "unavailable"),
            "restart_required"
        );
        assert_eq!(
            state(true, false, false, false, true, "unavailable"),
            "runtime_error"
        );
        assert_eq!(
            state(true, false, true, false, true, "unavailable"),
            "unsupported"
        );
        assert_eq!(
            state(true, false, true, true, false, "installed"),
            "missing_media"
        );
        assert_eq!(
            state(true, false, true, true, true, "download_on_first_use"),
            "model_missing"
        );
        assert_eq!(
            state(true, false, true, true, true, "installed"),
            "installed"
        );
    }
    use super::*;
    #[test]
    fn fixed_models_only() {
        assert!(selection("whisper/tiny").is_ok());
        assert!(selection("haas/haas-ja-v1.0").is_ok());
        for id in [
            "whisper/../../other",
            "auxiliary/https://evil/weights",
            "haas/C:\\evil.exe",
            "unknown/model",
        ] {
            assert!(selection(id).is_err());
        }
    }
}
