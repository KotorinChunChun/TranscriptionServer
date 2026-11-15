//! Bounded admission for independent inference processes. Estimates are reservations,
//! not an OS-enforced memory limit. Never share a worker's protocol stream across jobs.
use crate::{app::App, store::now};
use anyhow::{Context, Result, bail};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet};

pub const WORKERS: usize = 10;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub max_jobs: usize,
    pub realtime_reserved: usize,
    pub max_gpu_jobs: usize,
    /// Zero means automatic, never unlimited.
    #[serde(rename = "ramLimitMiB")]
    pub ram_limit_mib: u64,
    #[serde(rename = "vramLimitMiB")]
    pub vram_limit_mib: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_jobs: 10,
            realtime_reserved: 2,
            max_gpu_jobs: 2,
            ram_limit_mib: 0,
            vram_limit_mib: 0,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(2..=WORKERS).contains(&self.max_jobs)
            || self.realtime_reserved == 0
            || self.realtime_reserved >= self.max_jobs
            || self.max_gpu_jobs == 0
            || self.max_gpu_jobs > self.max_jobs
            || self.ram_limit_mib > 16 * 1024 * 1024
            || self.vram_limit_mib > 16 * 1024 * 1024
        {
            bail!(
                "並列数は2〜10、リアルタイム予約は1〜並列数未満、GPU並列数は1〜並列数です。メモリ上限も確認してください。"
            )
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    #[serde(rename = "ramMiB")]
    pub ram_mib: u64,
    #[serde(rename = "vramMiB")]
    pub vram_mib: u64,
}
impl std::ops::Add for Cost {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            ram_mib: self.ram_mib + rhs.ram_mib,
            vram_mib: self.vram_mib + rhs.vram_mib,
        }
    }
}
impl std::ops::Sub for Cost {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            ram_mib: self.ram_mib.saturating_sub(rhs.ram_mib),
            vram_mib: self.vram_mib.saturating_sub(rhs.vram_mib),
        }
    }
}

/// Conservative process + model + preprocessing envelopes, including CPU fallback.
/// These are deliberately separate from the UI's reference measurements.
pub fn estimate(engine: &str, model: &str, gpu: bool, diarize: bool) -> Option<Cost> {
    let (ram, vram) = match engine {
        "whisper" => match model {
            "tiny" | "base" => (2304, 1536),
            "small" => (3072, 3072),
            "medium" => (6144, 6144),
            "large-v3-turbo" | "turbo" => (8192, 7168),
            "large-v3" => (12288, 12288),
            _ => return None,
        },
        "haas" | "reazon" | "parakeet" | "nemotron-asr" => (5120, 5120),
        "qwen" => {
            if model.contains("1.7") {
                (10240, 10240)
            } else {
                (5120, 5120)
            }
        }
        "diarization" => (2304, 2304),
        "nemotron-transcribe" => (32768, 32768),
        "openai" | "compatible" => (512, 0),
        _ => return None,
    };
    let extra = if diarize && engine != "diarization" {
        2304
    } else {
        0
    };
    Some(Cost {
        ram_mib: ram + extra,
        vram_mib: if gpu { vram + extra } else { 0 },
    })
}
fn job_cost(job: &Value) -> Option<Cost> {
    estimate(
        job["engine"].as_str()?,
        job["modelId"].as_str()?,
        job["metadata"]["executionDevice"] == "gpu",
        job["diarizationEnabled"] == true,
    )
}

/// Total and currently available physical RAM, MiB. Unknown is fail-closed.
pub fn physical_ram() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut value = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: initialized structure with the size required by the Windows API.
        if unsafe { GlobalMemoryStatusEx(&mut value) } != 0 {
            return (value.ullTotalPhys / 1048576, value.ullAvailPhys / 1048576);
        }
    }
    (0, 0)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub job_id: String,
    pub model: String,
    pub workload: String,
    pub device: String,
    pub cost: Cost,
    pub quarantined: bool,
}
pub struct State {
    #[cfg(test)]
    pub test_memory: Option<(u64, u64)>,
    pub settings: Settings,
    pub active: HashMap<usize, Lease>,
    pub resident: Cost,
    pub waiting: HashMap<String, String>,
    pub detected_vram_mib: u64,
}
impl State {
    pub async fn new(settings: Settings) -> Result<Self> {
        settings.validate()?;
        // We do not select a GPU index. Use the smallest NVIDIA device's capacity,
        // so several GPUs never accidentally become one aggregate memory budget.
        let detected_vram_mib = {
            let output = tokio::time::timeout(
                Duration::from_secs(3),
                crate::platform::command(std::path::Path::new("nvidia-smi"))
                    .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
                    .output(),
            )
            .await;
            output
                .ok()
                .and_then(Result::ok)
                .filter(|v| v.status.success())
                .and_then(|v| {
                    String::from_utf8_lossy(&v.stdout)
                        .lines()
                        .filter_map(|s| s.trim().parse::<u64>().ok())
                        .min()
                })
                .unwrap_or(0)
        };
        Ok(Self {
            #[cfg(test)]
            test_memory: None,
            settings,
            active: HashMap::new(),
            resident: Cost::default(),
            waiting: HashMap::new(),
            detected_vram_mib,
        })
    }
    pub fn limits(&self) -> Cost {
        let total = self.memory().0;
        Cost {
            ram_mib: if self.settings.ram_limit_mib == 0 {
                total * 3 / 4
            } else {
                self.settings.ram_limit_mib.min(total)
            },
            vram_mib: if self.settings.vram_limit_mib == 0 {
                self.detected_vram_mib * 4 / 5
            } else if self.detected_vram_mib > 0 {
                self.settings.vram_limit_mib.min(self.detected_vram_mib)
            } else {
                self.settings.vram_limit_mib
            },
        }
    }
    fn auto_needs_cpu(&self, job: &Value) -> bool {
        job["device"] == "auto"
            && job["metadata"]["executionDevice"] == "gpu"
            && !self.gpu_capacity_known()
    }
    pub(crate) fn gpu_capacity_known(&self) -> bool {
        self.limits().vram_mib > 0
    }
    fn memory(&self) -> (u64, u64) {
        #[cfg(test)]
        if let Some(memory) = self.test_memory {
            return memory;
        }
        physical_ram()
    }
    fn used(&self) -> Cost {
        self.active
            .values()
            .fold(self.resident, |sum, lease| sum + lease.cost)
    }
    pub fn snapshot(&self) -> Value {
        json!({"settings":self.settings,"limits":self.limits(),"reserved":self.used(),
            "activeWorkers": self.active.iter().map(|(index, lease)| json!({"worker":index,"jobId":lease.job_id,
                "model":lease.model,"workload":lease.workload,"device":lease.device,"reservation":lease.cost,
                "quarantined":lease.quarantined})).collect::<Vec<_>>(),
            "waiting":self.waiting,"resident":self.resident,
            "availableRamMiB":self.memory().1,"detectedVramMiB":self.detected_vram_mib,"memoryPolicy":"estimated-admission",
            "priority":"realtime","preemption":false})
    }
    fn reason(
        &self,
        cost: Cost,
        reused: Cost,
        realtime: bool,
        file_waiting: bool,
    ) -> Option<&'static str> {
        if self.active.len() >= self.settings.max_jobs {
            return Some("並列数上限");
        }
        if !realtime
            && self
                .active
                .values()
                .filter(|v| v.workload == "file")
                .count()
                >= self.settings.max_jobs - self.settings.realtime_reserved
        {
            return Some("リアルタイム用の実行枠を予約中");
        }
        if realtime
            && file_waiting
            && self
                .active
                .values()
                .filter(|v| v.workload == "realtime")
                .count()
                >= self.settings.max_jobs - 1
        {
            return Some("ファイル処理用の実行枠を予約中");
        }
        if cost.vram_mib > 0
            && self.active.values().filter(|v| v.cost.vram_mib > 0).count()
                >= self.settings.max_gpu_jobs
        {
            return Some("GPU並列数上限");
        }
        self.memory_reason(cost, reused, !realtime)
    }
    fn memory_reason(
        &self,
        cost: Cost,
        reused: Cost,
        reserve_realtime: bool,
    ) -> Option<&'static str> {
        let mut limits = self.limits();
        if reserve_realtime {
            // Preserve a proportional budget for new realtime arrivals, not just slots.
            let share = (self.settings.max_jobs - self.settings.realtime_reserved) as u64;
            limits.ram_mib = limits.ram_mib * share / self.settings.max_jobs as u64;
            limits.vram_mib = limits.vram_mib * share / self.settings.max_jobs as u64;
        }
        let used = self.used() - reused + cost;
        if used.ram_mib > limits.ram_mib {
            return Some("RAM予算待ち");
        }
        if cost.vram_mib > 0 && limits.vram_mib == 0 {
            return Some("GPU容量未確認（VRAM上限を設定してください）");
        }
        if used.vram_mib > limits.vram_mib {
            return Some("VRAM予算待ち");
        }
        // Include not-yet-materialized reservations. Counting already allocated RAM
        // twice here is intentional/conservative; another process may also allocate.
        let pending = self
            .active
            .values()
            .fold(Cost::default(), |sum, l| sum + l.cost);
        if (pending + cost - reused).ram_mib + 1024 > self.memory().1 {
            return Some("OSのRAM空き待ち");
        }
        None
    }
}

struct Resident {
    index: usize,
    id: String,
    device: String,
    cost: Cost,
}
async fn residents(app: &App) -> Vec<Resident> {
    let busy: Vec<usize> = app
        .scheduler
        .lock()
        .unwrap()
        .active
        .keys()
        .copied()
        .collect();
    let mut result = vec![];
    for index in 0..WORKERS {
        if busy.contains(&index) {
            continue;
        }
        if let Ok(slot) = app.slots[index].try_lock()
            && let Some(s) = slot.as_ref()
        {
            result.push(Resident {
                index,
                id: format!("whisper/{}", s.model),
                device: s.device.clone(),
                cost: estimate("whisper", &s.model, s.device != "cpu", false).unwrap_or(Cost {
                    ram_mib: u64::MAX / 100,
                    vram_mib: u64::MAX / 100,
                }),
            });
        }
        if let Ok(slot) = app.native_slots[index].try_lock()
            && let Some(s) = slot.as_ref()
        {
            let (engine, model) = s.id.split_once('/').unwrap_or(("", ""));
            result.push(Resident {
                index,
                id: s.id.clone(),
                device: s.device.clone(),
                cost: estimate(engine, model, s.device != "cpu", false).unwrap_or(Cost {
                    ram_mib: u64::MAX / 100,
                    vram_mib: u64::MAX / 100,
                }),
            });
        }
    }
    app.scheduler.lock().unwrap().resident =
        result.iter().fold(Cost::default(), |sum, r| sum + r.cost);
    result
}

pub async fn status(app: &App) -> Value {
    // Bulk management already owns admission when it asks for a snapshot.
    if let Ok(_gate) = app.admission.try_lock() {
        residents(app).await;
    }
    app.scheduler.lock().unwrap().snapshot()
}

pub async fn configure(app: &App, value: Value) -> Result<Value> {
    use std::sync::atomic::Ordering;
    let settings: Settings = serde_json::from_value(value).context("並列処理設定が不正です")?;
    settings.validate()?;
    let _gate = app.admission.lock().await;
    if app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.update_busy.load(Ordering::Acquire)
        || app.incoming.load(Ordering::Acquire) > 0
        || !app.running.lock().unwrap().is_empty()
        || !app.store.active_jobs()?.is_empty()
        || !app.scheduler.lock().unwrap().active.is_empty()
    {
        bail!("CONFLICT: 処理・待機・転送・モデル操作・更新が完了してから設定してください")
    }
    residents(app).await;
    {
        let state = app.scheduler.lock().unwrap();
        let ram_limit = if settings.ram_limit_mib == 0 {
            state.memory().0 * 3 / 4
        } else {
            settings.ram_limit_mib.min(state.memory().0)
        };
        let vram_limit = if settings.vram_limit_mib == 0 {
            state.detected_vram_mib * 4 / 5
        } else if state.detected_vram_mib > 0 {
            settings.vram_limit_mib.min(state.detected_vram_mib)
        } else {
            settings.vram_limit_mib
        };
        if state.resident.ram_mib > ram_limit || state.resident.vram_mib > vram_limit {
            bail!("CONFLICT: 常駐モデルが新しいメモリ予算を超えます。先にアンロードしてください")
        }
    }
    let path = app.runtime.home.join("settings.json");
    let mut document = if path.exists() {
        crate::config::read_json(&path)?
    } else if app.runtime.installed {
        bail!("必須設定ファイルが欠落しています。保存を中止しました")
    } else {
        serde_json::to_value(&app.runtime.config)?
    };
    if !document.is_object() || document.as_object().is_none_or(|o| o.is_empty()) {
        bail!("既存設定が空または不正です。保存を中止しました")
    }
    document["execution"] = serde_json::to_value(&settings)?;
    crate::config::write_json(&path, &document)?;
    let mut state = app.scheduler.lock().unwrap();
    state.settings = settings;
    Ok(state.snapshot())
}

/// Public catalog counts intentionally exclude job IDs, client IDs and text.
pub fn model_counts(app: &App, id: &str) -> Value {
    let state = app.scheduler.lock().unwrap();
    let workers: Vec<_> = state
        .active
        .values()
        .filter(|l| l.model == id && !l.quarantined)
        .collect();
    json!({"running":workers.len(),"file":workers.iter().filter(|l|l.workload=="file").count(),
        "realtime":workers.iter().filter(|l|l.workload=="realtime").count()})
}

pub async fn check_manual(
    app: &App,
    lane: usize,
    engine: &str,
    model: &str,
    device: &str,
) -> Result<()> {
    let list = residents(app).await;
    let cost = estimate(engine, model, device != "cpu", false)
        .context("モデルのメモリ見積りがありません")?;
    let reused = list
        .iter()
        .filter(|r| r.index == lane)
        .fold(Cost::default(), |sum, r| sum + r.cost);
    if let Some(reason) = app
        .scheduler
        .lock()
        .unwrap()
        .memory_reason(cost, reused, false)
    {
        bail!("{reason}")
    }
    Ok(())
}

/// Release only this job's process tree, wait for confirmed termination before returning its budget.
pub async fn cleanup(app: &App, index: usize, keep_pinned: bool) -> Result<()> {
    let mut whisper = app.slots[index].lock().await;
    if let Some(s) = whisper.as_mut()
        && !(keep_pinned && s.pinned)
    {
        s.unload().await?;
        *whisper = None;
    }
    let mut native = app.native_slots[index].lock().await;
    if let Some(s) = native.as_mut()
        && !keep_pinned
    {
        s.unload().await?;
        *native = None;
    }
    Ok(())
}

pub async fn cleanup_scopes(app: &App, id: &str) -> Result<()> {
    let scopes = app
        .job_scopes
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .unwrap_or_default();
    for scope in &scopes {
        if !scope.empty()? {
            scope.terminate()?;
        }
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if scopes
                .iter()
                .map(|s| s.empty())
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|v| *v)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("ジョブ所有プロセスの終了未確認")??;
    app.job_scopes.lock().unwrap().remove(id);
    Ok(())
}

pub async fn wait_scope(scope: &crate::platform::Job) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !scope.empty()? {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("所有子プロセスの終了未確認")?
}

pub async fn dispatch(
    app: Arc<App>,
    mut file: mpsc::Receiver<String>,
    mut realtime: mpsc::Receiver<String>,
) {
    let mut queues: [VecDeque<String>; 2] = Default::default();
    let mut last_client = [String::new(), String::new()];
    let mut tasks: JoinSet<(usize, String, Result<()>)> = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = app.stop.cancelled() => break,
            Some(id) = file.recv() => queues[0].push_back(id),
            Some(id) = realtime.recv() => queues[1].push_back(id),
            Some(result) = tasks.join_next() => {
                if let Ok((index, id, cleanup)) = result { finish(&app,index,&id,cleanup); }
            },
            _ = tick.tick() => {},
        }
        while let Ok(id) = file.try_recv() {
            queues[0].push_back(id);
        }
        while let Ok(id) = realtime.try_recv() {
            queues[1].push_back(id);
        }
        for queue in &mut queues {
            queue.retain(|id| {
                let keep = app.store.get(id).is_ok_and(|j| j["status"] == "queued");
                if !keep {
                    app.running.lock().unwrap().remove(id);
                    app.scheduler.lock().unwrap().waiting.remove(id);
                }
                keep
            });
        }
        // Shares admission with create, management, settings, restart and updates.
        let _admission = app.admission.lock().await;
        if app.maintenance.load(std::sync::atomic::Ordering::Acquire) {
            continue;
        }
        let resident = residents(&app).await;
        for lane in [1, 0] {
            let mut ids = queues[lane].iter().cloned().collect::<Vec<_>>();
            // Stable partition: alternate clients while preserving order within a client.
            ids.sort_by_key(|id| {
                app.store
                    .get(id)
                    .ok()
                    .is_some_and(|j| j["clientId"] == last_client[lane])
            });
            for id in ids {
                let Ok(mut job) = app.store.get(&id) else {
                    continue;
                };
                if app.scheduler.lock().unwrap().auto_needs_cpu(&job) {
                    match app.store.update(&id,json!({"message":"GPU容量を確認できないためCPUで処理します。",
                        "metadata":{"executionDevice":"cpu","actualDevice":null,
                            "gpuFallback":true,"gpuFallbackBeforeExecution":true,
                            "gpuFallbackAt":now(),"gpuFallbackReasonCode":"gpu_capacity_unknown",
                            "gpuFallbackReason":"GPU容量を確認できずVRAM上限も未設定のためCPUを選択しました。",
                            "deviceWaitEnabled":false,"waitingReason":null}})) {
                        Ok(updated) => job=updated,
                        Err(_) => continue,
                    }
                }
                let model_id = format!(
                    "{}/{}",
                    job["engine"].as_str().unwrap_or(""),
                    job["modelId"].as_str().unwrap_or("")
                );
                let device = job["metadata"]["executionDevice"].as_str().unwrap_or("cpu");
                let cost = job_cost(&job);
                let allocation = {
                    let mut state = app.scheduler.lock().unwrap();
                    // A pinned anchor is used only by its workload and matching model/device.
                    let preferred = resident.iter().find(|r| {
                        r.index == lane
                            && r.id == model_id
                            && (r.device == device || (r.device == "cuda" && device == "gpu"))
                    });
                    let index = preferred
                        .map(|r| r.index)
                        .filter(|i| !state.active.contains_key(i))
                        .or_else(|| {
                            (0..WORKERS).find(|i| {
                                !state.active.contains_key(i)
                                    && !resident.iter().any(|r| r.index == *i)
                            })
                        });
                    let reused = index
                        .and_then(|i| resident.iter().find(|r| r.index == i))
                        .map(|r| r.cost)
                        .unwrap_or_default();
                    let reason = if let Some(cost) = cost {
                        state
                            .reason(cost, reused, lane == 1, !queues[0].is_empty())
                            .or(if index.is_none() {
                                Some("空きワーカー待ち")
                            } else {
                                None
                            })
                    } else {
                        Some("モデルのメモリ見積り未登録")
                    };
                    if let Some(reason) = reason {
                        let changed = state.waiting.get(&id).is_none_or(|s| s != reason);
                        state.waiting.insert(id.clone(), reason.into());
                        if changed {
                            let mut metadata =
                                json!({"waitingReason":reason,"deviceWaitEnabled":true});
                            if job["metadata"]["dequeuedAt"].is_null() {
                                metadata["dequeuedAt"] = now().into();
                            }
                            let _ = app.store.update(&id,json!({"phase":"waiting_resources","message":reason,"metadata":metadata}));
                        }
                        None
                    } else {
                        let index = index.unwrap();
                        let cost = cost.unwrap();
                        state.resident = state.resident - reused;
                        state.active.insert(
                            index,
                            Lease {
                                job_id: id.clone(),
                                model: model_id.clone(),
                                workload: if lane == 1 { "realtime" } else { "file" }.into(),
                                device: device.into(),
                                cost,
                                quarantined: false,
                            },
                        );
                        state.waiting.remove(&id);
                        Some(index)
                    }
                };
                if let Some(index) = allocation {
                    queues[lane].retain(|v| v != &id);
                    last_client[lane] = job["clientId"].as_str().unwrap_or("").into();
                    let _ = app.store.update(&id,json!({"metadata":{"waitingReason":null,"workerIndex":index,"memoryReservation":cost}}));
                    let target = app.clone();
                    tasks.spawn(async move {
                        let result = std::panic::AssertUnwindSafe(crate::jobs::execute(target.clone(),index,&id)).catch_unwind().await;
                        let ok = matches!(result,Ok(true));
                        if result.is_err() {
                            let _ = target.store.update(&id,json!({"status":"error","phase":"error","error":"認識ワーカーで内部エラーが発生しました"}));
                        }
                        let slots = cleanup(&target,index,ok).await;
                        let scopes = cleanup_scopes(&target,&id).await;
                        let cleanup = slots.and(scopes);
                        (index,id,cleanup)
                    });
                    // Re-evaluate fairness and residents on the next dispatch cycle.
                    break;
                }
            }
        }
    }
    for token in app.running.lock().unwrap().values() {
        token.cancel();
    }
    while let Some(result) = tasks.join_next().await {
        if let Ok((index, id, cleanup)) = result {
            finish(&app, index, &id, cleanup);
        }
    }
    for index in 0..WORKERS {
        let _ = cleanup(&app, index, false).await;
    }
    app.running.lock().unwrap().clear();
}
fn finish(app: &App, index: usize, id: &str, result: Result<()>) {
    let mut state = app.scheduler.lock().unwrap();
    if let Err(error) = result {
        // Keep the budget and slot quarantined if process exit could not be confirmed.
        if let Some(lease) = state.active.get_mut(&index) {
            lease.quarantined = true;
        }
        *app.worker_error.lock().unwrap() = Some(format!("終了未確認: {error}"));
    } else {
        state.active.remove(&index);
    }
    app.running.lock().unwrap().remove(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn state() -> State {
        let mut s = State::new(Settings {
            ram_limit_mib: 32768,
            vram_limit_mib: 8192,
            ..Default::default()
        })
        .await
        .unwrap();
        s.test_memory = Some((65536, 65536));
        s
    }
    fn lease(workload: &str, cost: Cost) -> Lease {
        Lease {
            job_id: "test".into(),
            model: "whisper/tiny".into(),
            device: "cpu".into(),
            workload: workload.into(),
            cost,
            quarantined: false,
        }
    }
    #[tokio::test]
    async fn automatic_cpu_selection_distinguishes_unknown_capacity_from_budget_waits() {
        let mut state = state().await;
        state.detected_vram_mib = 0;
        state.settings.vram_limit_mib = 0;
        let job = json!({"device":"auto","metadata":{"executionDevice":"gpu"}});
        assert!(state.auto_needs_cpu(&job));
        state.settings.vram_limit_mib = 4096;
        assert!(!state.auto_needs_cpu(&job));
        state.settings.vram_limit_mib = 0;
        state.detected_vram_mib = 8192;
        assert!(!state.auto_needs_cpu(&job));
        state.detected_vram_mib = 0;
        assert!(
            !state.auto_needs_cpu(&json!({"device":"gpu","metadata":{"executionDevice":"gpu"}}))
        );
        assert!(
            !state.auto_needs_cpu(&json!({"device":"auto","metadata":{"executionDevice":"cpu"}}))
        );
    }
    #[tokio::test]
    async fn reservations_cover_duplicates_residents_gpu_and_realtime_headroom() {
        let mut s = state().await;
        let cost = estimate("whisper", "tiny", false, false).unwrap();
        for i in 0..8 {
            assert!(s.reason(cost, Cost::default(), false, false).is_none());
            s.active.insert(i, lease("file", cost));
        }
        assert!(
            s.reason(cost, Cost::default(), false, false)
                .unwrap()
                .contains("リアルタイム")
        );
        assert!(s.reason(cost, Cost::default(), true, true).is_none());
        s.active.clear();
        s.resident = Cost {
            ram_mib: 30000,
            vram_mib: 0,
        };
        assert_eq!(
            s.reason(cost, Cost::default(), false, false),
            Some("RAM予算待ち")
        );
        s.resident = Cost::default();
        let gpu = estimate("haas", "haas-ja-v1.0", true, false).unwrap();
        s.active.insert(0, lease("realtime", gpu));
        assert_eq!(
            s.reason(gpu, Cost::default(), true, false),
            Some("VRAM予算待ち")
        );
        s.settings.vram_limit_mib = 0;
        assert!(
            s.reason(gpu, Cost::default(), true, false)
                .unwrap()
                .contains("GPU容量未確認")
        );
        s.active.clear();
        s.test_memory = Some((65536, 2000));
        assert_eq!(
            s.reason(cost, Cost::default(), true, false),
            Some("OSのRAM空き待ち")
        );
    }
    #[test]
    fn every_supported_local_model_has_an_envelope_unknown_models_fail_closed() {
        for (model, _) in crate::catalog::WHISPER_MODELS {
            assert!(
                estimate("whisper", model, false, false).is_some(),
                "{model}"
            );
        }
        for (engine, _, models) in crate::catalog::NATIVE {
            for (model, _) in models {
                assert!(
                    estimate(engine, model, false, true).is_some(),
                    "{engine}/{model}"
                );
            }
        }
        assert!(estimate("nemotron-asr", crate::nemo::MODEL, true, true).is_some());
        assert!(estimate("new-engine", "new-model", false, false).is_none());
        let base = estimate("whisper", "tiny", true, false).unwrap();
        let diar = estimate("whisper", "tiny", true, true).unwrap();
        assert!(diar.ram_mib > base.ram_mib && diar.vram_mib > base.vram_mib);
        assert!(
            Settings {
                max_jobs: 11,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                realtime_reserved: 10,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
