use crate::{
    catalog::{self, Tools},
    config::{Runtime, VERSION},
    jobs::Session,
    network::Policy,
    platform,
    store::{Store, active, now},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;
pub struct App {
    pub runtime: Runtime,
    pub store: Store,
    tools_snapshot: RwLock<Tools>,
    pub policy: Policy,
    pub engines: Mutex<Value>,
    pub management: Mutex<Value>,
    pub management_results: Mutex<Value>,
    pub model_inventory: Mutex<Value>,
    pub runtime_health: Mutex<Value>,
    pub runtime_pending_restart: Mutex<HashSet<String>>,
    pub runtime_setup: Mutex<Value>,
    pub runtime_storage: Mutex<Option<(std::time::Instant, Value)>>,
    pub management_cancel: Mutex<CancellationToken>,
    pub native_slots: Vec<AsyncMutex<Option<crate::models::NativeSession>>>,
    pub token: String,
    pub key: String,
    pub admission: AsyncMutex<()>,
    pub maintenance: AtomicBool,
    pub draining: AtomicBool,
    pub drain_state: Mutex<Value>,
    pub drain_cancel: Mutex<CancellationToken>,
    pub incoming: AtomicUsize,
    pub stop: CancellationToken,
    pub restart_requested: AtomicBool,
    pub supervisor: platform::Job,
    pub pending: [mpsc::Sender<String>; 2],
    pub running: Mutex<HashMap<String, CancellationToken>>,
    pub slots: Vec<AsyncMutex<Option<Session>>>,
    pub scheduler: Mutex<crate::scheduler::State>,
    pub job_scopes: Mutex<HashMap<String, Vec<Arc<platform::Job>>>>,
    pub worker_error: Mutex<Option<String>>,
    pub update_state: Mutex<Value>,
    pub update_token: Mutex<String>,
    pub update_busy: AtomicBool,
}
impl App {
    #[cfg(test)]
    pub async fn new(runtime: Runtime) -> Result<Arc<Self>> {
        Self::initialize(runtime, None, CancellationToken::new()).await
    }
    pub async fn start(runtime: Runtime, stop: CancellationToken) -> Result<Arc<Self>> {
        Self::start_with(runtime, stop, crate::runtime_bootstrap::install_runtime).await
    }
    pub(crate) async fn start_with(
        runtime: Runtime,
        stop: CancellationToken,
        installer: crate::runtime_bootstrap::Installer,
    ) -> Result<Arc<Self>> {
        let app = Self::initialize_fenced(runtime, None, stop, true).await?;
        app.maintenance.store(true, Ordering::Release);
        *app.runtime_setup.lock().unwrap() = json!({"state":"preparing","modelDownloads":false});
        let task = app.clone();
        tokio::spawn(async move {
            let result = crate::runtime_bootstrap::prepare(&task, installer).await;
            if let Err(error) = result {
                if !task.stop.is_cancelled() {
                    let _ = crate::runtime_bootstrap::activate(&task).await;
                }
                *task.runtime_setup.lock().unwrap() = json!({"state":"incomplete","message":error.to_string(),"modelDownloads":task.runtime.config.auto_install_runtimes});
                *task.management.lock().unwrap() =
                    json!({"state":"error","message":error.to_string()});
            }
            task.maintenance.store(false, Ordering::Release);
        });
        Ok(app)
    }
    pub fn tools(&self) -> Tools {
        self.tools_snapshot.read().unwrap().clone()
    }
    pub(crate) fn replace_tools(&self, tools: Tools) {
        *self.tools_snapshot.write().unwrap() = tools;
    }
    pub fn preparing(&self) -> bool {
        self.runtime_setup.lock().unwrap()["state"] == "preparing"
    }
    #[cfg(test)]
    pub(crate) async fn initialize(
        runtime: Runtime,
        installer: Option<crate::runtime_bootstrap::Installer>,
        stop: CancellationToken,
    ) -> Result<Arc<Self>> {
        Self::initialize_fenced(runtime, installer, stop, false).await
    }
    async fn initialize_fenced(
        runtime: Runtime,
        installer: Option<crate::runtime_bootstrap::Installer>,
        stop: CancellationToken,
        startup_pending: bool,
    ) -> Result<Arc<Self>> {
        let policy = Policy::new(&runtime.config)?;
        let key = std::env::var("MEETING_SERVER_API_KEY").unwrap_or_default();
        if runtime.config.server
            && (key.len() < 32 || !key.bytes().all(|b| (33..=126).contains(&b)))
        {
            bail!("Bearerモードは32文字以上のMEETING_SERVER_API_KEYが必要です")
        }
        let store = Store::open(&runtime.config.data_dir)?;
        let supervisor = platform::Job::new()?;
        let tools = Tools::resolve(&runtime);
        let engines = catalog::discover(&tools, &supervisor).await;
        let (tx0, rx0) = mpsc::channel(256);
        let (tx1, rx1) = mpsc::channel(256);
        let scheduler = crate::scheduler::State::new(runtime.config.execution.clone()).await?;
        let app = Arc::new(Self {
            runtime,
            store,
            tools_snapshot: RwLock::new(tools),
            policy,
            engines: Mutex::new(engines),
            management: Mutex::new(json!({"state":"idle","message":""})),
            management_results: Mutex::new(json!({})),
            model_inventory: Mutex::new(json!([])),
            runtime_health: Mutex::new(json!({})),
            runtime_pending_restart: Mutex::new(HashSet::new()),
            runtime_setup: Mutex::new(json!({"state":"not_run"})),
            runtime_storage: Mutex::new(None),
            management_cancel: Mutex::new(CancellationToken::new()),
            native_slots: (0..crate::scheduler::WORKERS)
                .map(|_| AsyncMutex::new(None))
                .collect(),
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            key,
            admission: AsyncMutex::new(()),
            maintenance: AtomicBool::new(startup_pending),
            draining: AtomicBool::new(false),
            drain_state: Mutex::new(json!({"state":"idle"})),
            drain_cancel: Mutex::new(CancellationToken::new()),
            incoming: AtomicUsize::new(0),
            stop,
            restart_requested: AtomicBool::new(false),
            supervisor,
            pending: [tx0, tx1],
            running: Mutex::new(HashMap::new()),
            slots: (0..crate::scheduler::WORKERS)
                .map(|_| AsyncMutex::new(None))
                .collect(),
            scheduler: Mutex::new(scheduler),
            job_scopes: Mutex::new(HashMap::new()),
            worker_error: Mutex::new(None),
            update_state: Mutex::new(
                json!({"currentVersion":VERSION,"available":null,"state":"idle","lastChecked":null,"message":""}),
            ),
            update_token: Mutex::new(String::new()),
            update_busy: AtomicBool::new(false),
        });
        crate::models::refresh(&app).await;
        if let Some(installer) = installer {
            crate::runtime_bootstrap::prepare(&app, installer).await?;
        }
        tokio::spawn(crate::scheduler::dispatch(app.clone(), rx0, rx1));
        let mut recovered = app.store.active_jobs()?;
        recovered.reverse();
        for job in recovered
            .into_iter()
            .filter(|job| job["status"] == "queued")
        {
            let id = job["id"]
                .as_str()
                .context("復旧ジョブIDがありません")?
                .to_string();
            app.running
                .lock()
                .unwrap()
                .insert(id.clone(), CancellationToken::new());
            let lane = usize::from(job["workload"] == "realtime");
            app.pending[lane]
                .send(id)
                .await
                .context("復旧キューが停止しています")?;
        }
        Ok(app)
    }
    pub fn job_scope(&self, id: &str) -> Result<Arc<platform::Job>> {
        let scope = Arc::new(platform::Job::new()?);
        let mut scopes = self.job_scopes.lock().unwrap();
        let list = scopes.entry(id.into()).or_default();
        list.retain(|s| Arc::strong_count(s) > 1 || !s.empty().unwrap_or(false));
        list.push(scope.clone());
        Ok(scope)
    }
    pub fn health(&self) -> Value {
        let engines = self.engines.lock().unwrap().clone();
        let ready = engines
            .as_array()
            .is_some_and(|v| v.iter().any(|e| e["ready"] == true));
        let config = &self.runtime.config;
        let target = if config.lan || config.server {
            "server"
        } else {
            "local"
        };
        let auth = if config.server {
            "bearer"
        } else if config.lan {
            "lan"
        } else {
            "local"
        };
        let reason = if self.preparing() {
            "実行環境を準備しています。完了後に受付を開始します。"
        } else if ready {
            ""
        } else {
            "認識エンジンを準備してください"
        };
        let capabilities = json!({"modelManagement":true,"updates":true,
            "jobClassification":true,"jobPaging":true,"runtimeStatus":true,"mediaCleanup":true,
            "realtimeLane":true,"realtimeMaxSeconds":30,"whisperReuse":true,"bulkSegmentEdit":true,
            "engineSelection":true,"modelDiscovery":true,"openaiAudio":true,"asynchronousJobs":true});
        let preparing = self.preparing();
        let mut result = json!({"ok":true,"ready":ready&&!preparing,"appId":"meeting-transcriber",
            "protocolVersion":3,"serverVersion":VERSION,"runtime":"rust",
            "processId":std::process::id(),"engine":"speech.cpp","model":"Haas-ja-v1.0",
            "modelId":"haas-ja-v1.0","version":"Rust core",
            "speechAvailable":self.tools().speech.is_some(),
            "ffmpegAvailable":self.tools().ffmpeg.is_some(),
            "ffprobeAvailable":self.tools().ffprobe.is_some(),
            "executionTarget":target,"authMode":auth,"capabilities":capabilities,
            "engines":engines,"reason":reason,
            "downloadNote":"不足する実行環境は起動時に準備します（無効化設定あり）。モデル本体は起動時に取得せず、管理画面から導入します。一体型は設定で許可した場合のみ事前導入します。"});
        result["capabilities"]["automaticRuntimeSetup"] = true.into();
        result["startupState"] = self.runtime_setup.lock().unwrap()["state"].clone();
        result["acceptingJobs"] = (!self.maintenance.load(Ordering::Acquire)
            && !self.draining.load(Ordering::Acquire)
            && !self.stop.is_cancelled())
        .into();
        result["capabilities"]["gracefulDrain"] = true.into();
        result["lifecycle"] = crate::lifecycle::status(self);
        result["capabilities"]["runtimeStorage"] = true.into();
        result["capabilities"]["standaloneDiarization"] = true.into();
        result["capabilities"]["jobTiming"] = true.into();
        result["capabilities"]["serverRestart"] = true.into();
        result["capabilities"]["automaticGpuFallback"] = true.into();
        result["capabilities"]["concurrentInference"] = true.into();
        result["capabilities"]["restartRecovery"] = true.into();
        result["recoveryPolicy"] = json!({"mode":"same-id-replay","maxRecoveryCount":3,"externalApiReplay":false,"resultGeneration":"metadata.resultGeneration"});
        result["capabilities"]["resourceBudgetScheduling"] = true.into();
        result["schedulingUrl"] = "/api/scheduling".into();
        result["capabilities"]["modelReadiness"] = true.into();
        result["capabilities"]["https"] = self.runtime.config.tls_cert.is_some().into();
        result["transport"] = json!({"https":self.runtime.config.tls_cert.is_some(),"http":!self.runtime.config.https_only});
        result["capabilities"]["modelInformation"] = true.into();
        result["modelInformationUrl"] = "/api/models".into();
        result["capabilities"]["jobDeletion"] = true.into();
        result["capabilities"]["sourceMediaDigest"] = true.into();
        result["capabilities"]["mediaDeduplication"] = true.into();
        result["mediaDeduplicationMode"] = "hardlink_when_supported".into();
        result["instanceId"] = self.runtime.config.instance_id.clone().into();
        if !self.runtime.config.server {
            result["token"] = self.token.clone().into()
        }
        if !self.runtime.config.server && !self.runtime.config.lan {
            result["dataDirectory"] = self.store.dir.to_string_lossy().into_owned().into()
        }
        result
    }
    pub fn selected(
        &self,
        engine: &str,
        model: &str,
        device: &str,
        diar: bool,
    ) -> Result<(Value, Value)> {
        if engine == crate::integrated::ENGINE {
            crate::integrated::check_enabled(self)?;
        }
        let runtime = match engine {
            "whisper" => "whisper",
            crate::nemo::ENGINE => "nemo",
            "openai" | "compatible" => "provider",
            _ => "speech",
        };
        let pending = self.runtime_pending_restart.lock().unwrap();
        if pending.contains(runtime)
            || pending.contains("ffmpeg")
            || (diar && pending.contains("speech"))
        {
            bail!("UNAVAILABLE: 実行環境の反映にはサーバーの再起動が必要です。")
        }
        drop(pending);
        let engines = self.engines.lock().unwrap().clone();
        let e = engines
            .as_array()
            .context("モデル一覧が不正です")?
            .iter()
            .find(|e| e["id"] == engine)
            .context("選択したエンジンは未対応です")?;
        if e["ready"] != true {
            bail!(
                "UNAVAILABLE: {}",
                e["reason"].as_str().unwrap_or("認識環境が未準備です")
            )
        }
        let models = e["models"].as_array().context("モデルがありません")?;
        let m = if model.is_empty() {
            models.first()
        } else {
            models.iter().find(|m| m["id"] == model)
        }
        .context("選択したモデルは未対応です")?;
        if !e["processingMethods"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == device))
        {
            bail!("選択した処理方法は利用できません")
        }
        if diar && m["diarization"] != true {
            bail!("このモデルで話者識別は利用できません")
        };
        Ok((e.clone(), m.clone()))
    }
    pub async fn create(&self, query: &HashMap<String, String>) -> Result<Value> {
        self.create_admitted(query, false).await
    }
    pub(crate) async fn create_admitted(
        &self,
        query: &HashMap<String, String>,
        incoming_accepted: bool,
    ) -> Result<Value> {
        let _gate = self.admission.lock().await;
        if self.maintenance.load(Ordering::Acquire)
            || (self.draining.load(Ordering::Acquire) && !incoming_accepted)
        {
            bail!("UNAVAILABLE: 更新またはモデル操作のため新規受付を停止しています")
        }
        if self.store.active_jobs()?.len() >= 256 {
            bail!("UNAVAILABLE: 処理件数の上限です。完了を待ってください")
        }
        let get =
            |key: &str, default: &str| query.get(key).cloned().unwrap_or_else(|| default.into());
        let engine = get("engine", "haas");
        let model = get("model", "");
        let device = get("device", "auto");
        let diar = get("diarize", "true");
        if !matches!(diar.as_str(), "true" | "false") {
            bail!("話者識別指定が不正です")
        };
        let diar = diar == "true";
        if engine == "diarization" && !diar {
            bail!("単独話者分離ではdiarize=falseを指定できません")
        }
        let (e, m) = self.selected(&engine, &model, &device, diar)?;
        let recognition_options = if e["externalApi"] == true {
            let cfg = catalog::provider(&engine, m["id"].as_str().unwrap_or(""))?;
            if query
                .get("language")
                .is_some_and(|v| v != &cfg.language && !(v == "auto" && cfg.language.is_empty()))
            {
                return Err(crate::recognition::unsupported("language"));
            }
            let mut local = query.clone();
            local.remove("language");
            let mut options = crate::recognition::validate(&engine, &local)?;
            options["language"] = if cfg.language.is_empty() {
                "auto".into()
            } else {
                cfg.language.into()
            };
            options
        } else {
            crate::recognition::validate(&engine, query)?
        };
        if engine == "whisper" {
            for key in ["language", "prompt", "temperature"] {
                let requires = if key == "language" {
                    recognition_options[key] != "ja"
                } else {
                    !recognition_options[key].is_null()
                };
                if requires
                    && !e["requestOptions"]
                        .as_array()
                        .is_some_and(|v| v.iter().any(|v| v == key))
                {
                    return Err(crate::recognition::unsupported(key));
                }
            }
        }
        if engine == crate::integrated::ENGINE {
            crate::integrated::require_model(&self.tools(), &self.supervisor).await?;
        }
        if engine == "diarization" {
            catalog::require_diarization_file(&self.tools(), &self.supervisor).await?;
        }
        if engine == crate::nemo::ENGINE {
            crate::nemo::require_model(&self.tools())?;
            if diar {
                catalog::require_diarization_file(&self.tools(), &self.supervisor).await?;
            }
        }
        let start: f64 = get("start", "0").parse()?;
        let end = query
            .get("end")
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<f64>())
            .transpose()?;
        let name = get("name", "recording")
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .chars()
            .take(255)
            .collect::<String>();
        let client = get("clientId", "unknown");
        let lane = get("workload", "file");
        if name.is_empty()
            || !start.is_finite()
            || start < 0.
            || end.is_some_and(|v| !v.is_finite() || v <= start)
            || client.is_empty()
            || client.len() > 64
            || !client
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !matches!(lane.as_str(), "file" | "realtime")
        {
            bail!("ファイル名/処理範囲/アプリ分類が不正です")
        }
        let target = if self.runtime.config.lan || self.runtime.config.server {
            "server"
        } else {
            "local"
        };
        let execution_device = if device == "auto" && e["externalApi"] != true {
            if e["processingMethods"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["id"] == "gpu"))
            {
                "gpu"
            } else {
                "cpu"
            }
        } else {
            device.as_str()
        };
        let external = e["externalApi"] == true;
        let diarization_model = if diar {
            m["diarizationModel"].clone()
        } else {
            Value::Null
        };
        let diarization_status = if diar { "pending" } else { "not_performed" };
        let transferred_to = if external { engine.as_str() } else { target };
        let metadata = json!({"engine":e["runtime"],"engineId":engine,"model":m["label"],
            "modelId":m["id"],"language":recognition_options["language"],
            "recognitionOptions":recognition_options,"timestampType":m["timestampType"],
            "diarizationModel":diarization_model,"diarizationStatus":diarization_status,
            "executionTarget":target,"mediaUploaded":false,"localHelperTransfer":target=="local",
            "mediaTransferredTo":transferred_to,"externalApi":external,
            "processingStatus":"uploading","executionDevice":execution_device,
            "apiConnectionTarget":target});
        let mut job = json!({"fileName":name,"status":"uploading","phase":"uploading",
            "message":"音声をサーバーに保存しています。","segmentCount":0,"duration":null,
            "processedUntil":start,"rangeStart":start,"rangeEnd":end,"diarizationEnabled":diar,
            "device":device,"engine":engine,"modelId":m["id"],"clientId":client,"workload":lane,
            "error":null,"metadata":metadata});
        job["task"] = e["task"].as_str().unwrap_or("transcription").into();
        job["metadata"]["task"] = job["task"].clone();
        if engine == crate::nemo::ENGINE {
            job["metadata"]["language"] =
                crate::nemo::language(query.get("language").map(String::as_str))?.into();
        }
        self.store.create(job)
    }
    pub async fn enqueue(&self, id: &str) -> Result<Value> {
        if !active(&self.store.get(id)?) {
            bail!("アップロード中に処理を中止しました")
        };
        let job = self.store.update(
            id,
            json!({"status":"queued","phase":"queued","message":"順番待ちです。",
                "metadata":{"queuedAt":now()}}),
        )?;
        let token = CancellationToken::new();
        self.running.lock().unwrap().insert(id.into(), token);
        let lane = usize::from(job["workload"] == "realtime");
        self.pending[lane]
            .send(id.into())
            .await
            .map_err(|_| anyhow::anyhow!("処理枠が停止しています"))?;
        Ok(job)
    }
    pub fn cancel(&self, id: &str) -> Result<Value> {
        let job = self.store.get(id)?;
        if !active(&job) {
            return Ok(job);
        }
        if let Some(token) = self.running.lock().unwrap().get(id) {
            token.cancel()
        };
        let diarization = if job["diarizationEnabled"] == true {
            "partial"
        } else {
            "not_performed"
        };
        self.store.update(
            id,
            json!({"status":"cancelled","phase":"cancelled",
                "message":"停止しました。確定済みの字幕は保持しています。",
                "metadata":{"diarizationStatus":diarization}}),
        )
    }
    pub async fn status(&self) -> Result<Value> {
        let jobs = self.store.active_jobs()?;
        let mut models = vec![];
        for lane in 0..2 {
            if let Ok(slot) = self.slots[lane].try_lock()
                && let Some(slot) = slot.as_ref()
            {
                let name = if lane == 0 { "file" } else { "realtime" };
                models.push(json!({"lane":name,"model":slot.model,"device":slot.device,
                    "busy":false,"requests":slot.requests}))
            }
        }
        let alive = !self.stop.is_cancelled();
        let queued = |realtime: bool| {
            jobs.iter()
                .filter(|j| j["status"] == "queued" && (j["workload"] == "realtime") == realtime)
                .count()
        };
        let running = jobs
            .iter()
            .filter(|j| j["status"] == "running")
            .collect::<Vec<_>>();
        Ok(
            json!({"workers":[{"lane":"file","alive":alive},{"lane":"realtime","alive":alive}],
            "queued":{"file":queued(false),"realtime":queued(true)},"running":running,
            "models":models,"lastWorkerError":self.worker_error.lock().unwrap().clone(),
            "gpuPolicy":"budgeted-parallel","realtimeMaxSeconds":30,
            "scheduling":self.scheduler.lock().unwrap().snapshot()}),
        )
    }
    pub fn print_startup(&self) {
        println!("TranscriptionServer - v{VERSION} / Rust core");
        for url in self.policy.urls(&self.runtime.config.host) {
            println!("別PCからの接続: {url}")
        }
        println!(
            "このPCの管理: {}://127.0.0.1:{}/",
            if self.policy.https { "https" } else { "http" },
            self.runtime.config.port
        );
        if self.runtime.config.lan {
            println!(
                "接続: 信頼LAN・キー入力不要 / 許可範囲: {}",
                self.policy
                    .networks
                    .iter()
                    .map(|n| n.cidr.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else {
            println!(
                "接続: {}",
                if self.runtime.config.server {
                    "Bearer認証"
                } else {
                    "このPCのみ"
                }
            );
        }
        println!("モデル点検（重みの取得・実推論は実施しません）:");
        let engines = self.engines.lock().unwrap().clone();
        for e in engines.as_array().into_iter().flatten() {
            let label = e["label"].as_str().unwrap_or("");
            if e["ready"] != true {
                println!(
                    "  {label}: 実行不可 / {}",
                    e["reason"].as_str().unwrap_or("未準備")
                );
                continue;
            }
            let methods = e["processingMethods"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["id"] != "auto")
                .filter_map(|m| m["label"].as_str())
                .collect::<Vec<_>>()
                .join("/");
            for m in e["models"].as_array().into_iter().flatten() {
                println!(
                    "  {label} / {}: {} / {methods}{}",
                    m["label"].as_str().unwrap_or(""),
                    match m["availability"].as_str() {
                        Some("installed") => "導入済み",
                        Some("provider") => "外部API・利用枠未確認",
                        _ => "初回取得",
                    },
                    if m["diarization"] == true {
                        " / 話者識別候補あり"
                    } else {
                        ""
                    }
                )
            }
        }
        println!(
            "GPU候補の検出と実GPU推論の成功は別です。\nデータ: {}",
            self.store.dir.display()
        );
    }
}
