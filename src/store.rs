use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
};
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn active(job: &Value) -> bool {
    matches!(
        job["status"].as_str(),
        Some("uploading" | "queued" | "running")
    )
}
pub fn id_valid(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub struct Store {
    pub dir: PathBuf,
    db: Mutex<Connection>,
    _guard: File,
}
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let guard = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(".service.lock"))?;
        guard
            .try_lock_exclusive()
            .context("保存先は他のサーバーが使用中です")?;
        let db = Connection::open(dir.join("workbench.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(15))?;
        // WAL+NORMAL: アプリの異常終了ではコミット済みを失わず、字幕1行ごとのfsyncを避ける。
        db.execute_batch(concat!(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; ",
            "CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,payload TEXT NOT NULL);",
            "CREATE TABLE IF NOT EXISTS segments(job_id TEXT NOT NULL,id INTEGER NOT NULL,",
            "payload TEXT NOT NULL,PRIMARY KEY(job_id,id));",
            "CREATE TABLE IF NOT EXISTS job_index(id TEXT PRIMARY KEY,created_at TEXT NOT NULL,",
            "status TEXT NOT NULL,client_id TEXT NOT NULL,workload TEXT NOT NULL);",
            "CREATE INDEX IF NOT EXISTS jobs_created ON job_index(created_at DESC,id DESC);",
            "CREATE INDEX IF NOT EXISTS jobs_client ON job_index(client_id,created_at DESC);",
        ))?;
        let store = Self {
            dir: dir.to_path_buf(),
            db: Mutex::new(db),
            _guard: guard,
        };
        store.db.lock().unwrap().execute_batch(concat!(
            "CREATE TABLE IF NOT EXISTS gpu_attempt_segments(job_id TEXT NOT NULL,",
            "id INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(job_id,id));",
            "CREATE TABLE IF NOT EXISTS restart_attempts(job_id TEXT NOT NULL,generation INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(job_id,generation));",
            "CREATE TABLE IF NOT EXISTS restart_segments(job_id TEXT NOT NULL,generation INTEGER NOT NULL,kind TEXT NOT NULL,id INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(job_id,generation,kind,id));",
        ))?;
        store.db.lock().unwrap().execute_batch(concat!(
            "CREATE INDEX IF NOT EXISTS jobs_media_digest ON jobs(",
            "json_extract(payload,'$.sourceSha256'),json_extract(payload,'$.sourceBytes'));",
        ))?;
        store.recover_media_operations()?;
        {
            let db = store.db.lock().unwrap();
            let mut stmt =
                db.prepare("SELECT payload FROM jobs WHERE id NOT IN(SELECT id FROM job_index)")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let job: Value = serde_json::from_str(&row?)?;
                Self::index(&db, &job)?;
            }
        }
        for job in store.list(None, None)?.into_iter().filter(active) {
            if store.recover_for_restart(&job)? {
                continue;
            }
            if let Some(stages) = job["metadata"]["measurements"]["stages"].as_array() {
                for value in stages.iter().filter(|s| s["status"] == "running") {
                    let mut stage = value.clone();
                    stage["status"] = "interrupted".into();
                    // A crash provides no trustworthy monotonic finish time.
                    stage["finishedAt"] = Value::Null;
                    stage["elapsedSeconds"] = Value::Null;
                    stage["interruptionReason"] = "server_terminated_before_stage_finish".into();
                    store.record_stage(job["id"].as_str().unwrap(), stage)?;
                }
            }
            store.update(
                job["id"].as_str().unwrap(),
                json!({"status":"interrupted","phase":"interrupted",
                    "message":"サーバーが再起動しました。確定済みの字幕は保持しています。",
                    "metadata":{"finishedAt":job["updatedAt"],"timingEstimated":true}}),
            )?;
        }
        Ok(store)
    }
    fn index(db: &Connection, job: &Value) -> Result<()> {
        db.execute(
            "INSERT OR REPLACE INTO job_index VALUES(?,?,?,?,?)",
            params![
                job["id"].as_str(),
                job["createdAt"].as_str(),
                job["status"].as_str(),
                job["clientId"].as_str().unwrap_or("unknown"),
                job["workload"].as_str().unwrap_or("file")
            ],
        )?;
        Ok(())
    }
    fn get_db(db: &Connection, id: &str) -> Result<Value> {
        if !id_valid(id) {
            bail!("NOT_FOUND")
        };
        let raw: String = db
            .query_row("SELECT payload FROM jobs WHERE id=?", [id], |r| r.get(0))
            .map_err(|_| anyhow::anyhow!("NOT_FOUND"))?;
        Ok(serde_json::from_str(&raw)?)
    }
    fn put(db: &Connection, job: &mut Value) -> Result<()> {
        job["updatedAt"] = now().into();
        job["revision"] = (job["revision"].as_u64().unwrap_or(0) + 1).into();
        db.execute(
            "INSERT OR REPLACE INTO jobs VALUES(?,?)",
            params![job["id"].as_str(), serde_json::to_string(job)?],
        )?;
        Self::index(db, job)
    }
    pub fn create(&self, mut job: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        job["id"] = id.clone().into();
        job["createdAt"] = now().into();
        if !job["metadata"].is_object() {
            job["metadata"] = json!({});
        }
        job["metadata"]["resultGeneration"] = 0.into();
        if active(&job) {
            job["metadata"]["timingVersion"] = 1.into();
        }
        fs::create_dir(self.dir.join(&id))?;
        let db = self.db.lock().unwrap();
        Self::put(&db, &mut job)?;
        Ok(crate::timing::snapshot(job))
    }
    fn recover_for_restart(&self, previous: &Value) -> Result<bool> {
        let id = previous["id"].as_str().context("ジョブIDがありません")?;
        let engine = previous["engine"].as_str().unwrap_or("");
        let local = matches!(
            engine,
            "whisper"
                | "haas"
                | "qwen"
                | "reazon"
                | "parakeet"
                | "nemotron-asr"
                | "nemotron-transcribe"
                | "diarization"
        );
        if !local
            || previous["metadata"]["externalApi"] == true
            || previous["metadata"]["mediaUploadComplete"] != true
        {
            return Ok(false);
        }
        let recovery_count = previous["metadata"]["recoveryCount"].as_u64().unwrap_or(0);
        let validation = (|| -> Result<()> {
            if recovery_count >= 3 {
                bail!("自動再実行の上限3回に達しました")
            }
            let model = previous["modelId"]
                .as_str()
                .context("モデルIDがありません")?;
            if engine != "diarization" {
                crate::models::selection(&format!("{engine}/{model}"))?;
            }
            if !matches!(previous["workload"].as_str(), Some("file" | "realtime")) {
                bail!("処理用途が不正です")
            }
            let path = regular_job_file(&self.dir, id, "source.media")?;
            let bytes = previous["sourceBytes"]
                .as_u64()
                .context("音声容量の記録がありません")?;
            if bytes == 0 || fs::metadata(&path)?.len() != bytes {
                bail!("保存音声の容量が一致しません")
            }
            let expected = previous["sourceSha256"]
                .as_str()
                .context("音声SHA256の記録がありません")?;
            use sha2::{Digest, Sha256};
            let mut file = File::open(path)?;
            let mut hash = Sha256::new();
            std::io::copy(&mut file, &mut hash)?;
            if hex::encode(hash.finalize()) != expected {
                bail!("保存音声のSHA256が一致しません")
            }
            Ok(())
        })();
        if let Err(error) = validation {
            self.update(id,json!({"status":"interrupted","phase":"interrupted","message":format!("自動再実行できません: {error}"),
                "metadata":{"recoveryBlockedReason":error.to_string(),"finishedAt":previous["updatedAt"],"timingEstimated":true}}))?;
            return Ok(true);
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        let generation = job["metadata"]["resultGeneration"].as_u64().unwrap_or(0);
        tx.execute(
            "INSERT INTO restart_attempts VALUES(?,?,?)",
            params![id, generation, serde_json::to_string(&job)?],
        )?;
        tx.execute("INSERT INTO restart_segments SELECT job_id,?,'confirmed',id,payload FROM segments WHERE job_id=?",params![generation,id])?;
        tx.execute("INSERT INTO restart_segments SELECT job_id,?,'gpu',id,payload FROM gpu_attempt_segments WHERE job_id=?",params![generation,id])?;
        tx.execute("DELETE FROM segments WHERE job_id=?", [id])?;
        tx.execute("DELETE FROM gpu_attempt_segments WHERE job_id=?", [id])?;
        let mut metadata = job["restartReplay"]["metadata"]
            .as_object()
            .cloned()
            .unwrap_or_else(|| {
                let keys = [
                    "engine",
                    "engineId",
                    "model",
                    "modelId",
                    "language",
                    "recognitionOptions",
                    "timestampType",
                    "diarizationModel",
                    "executionTarget",
                    "externalApi",
                    "executionDevice",
                    "task",
                    "apiConnectionTarget",
                    "mediaTransferredTo",
                    "localHelperTransfer",
                ];
                keys.into_iter()
                    .filter_map(|k| job["metadata"].get(k).map(|v| (k.to_string(), v.clone())))
                    .collect()
            });
        metadata.insert("resultGeneration".into(), (generation + 1).into());
        metadata.insert("recoveryCount".into(), (recovery_count + 1).into());
        metadata.insert("recoveredAt".into(), now().into());
        metadata.insert("recoveryReason".into(), "server_restart".into());
        metadata.insert("queuedAt".into(), now().into());
        metadata.insert("mediaUploaded".into(), true.into());
        metadata.insert("mediaUploadComplete".into(), true.into());
        metadata.insert("processingStatus".into(), "queued".into());
        metadata.insert(
            "diarizationStatus".into(),
            if job["diarizationEnabled"] == true {
                "pending"
            } else {
                "not_performed"
            }
            .into(),
        );
        job["metadata"] = metadata.into();
        job["status"] = "queued".into();
        job["phase"] = "recovered".into();
        job["message"] = "サーバー再起動後、同じジョブIDで先頭から再実行します。".into();
        job["segmentCount"] = 0.into();
        job["processedUntil"] = job["rangeStart"].as_f64().unwrap_or(0.).into();
        job["duration"] = Value::Null;
        job["error"] = Value::Null;
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn get(&self, id: &str) -> Result<Value> {
        Self::get_db(&self.db.lock().unwrap(), id).map(crate::timing::snapshot)
    }
    pub fn list(&self, client: Option<&str>, lane: Option<&str>) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(concat!(
            "SELECT j.payload FROM jobs j JOIN job_index i ON j.id=i.id ",
            "WHERE (?1 IS NULL OR i.client_id=?1) AND (?2 IS NULL OR i.workload=?2) ",
            "ORDER BY i.created_at DESC,i.id DESC",
        ))?;
        stmt.query_map(params![client, lane], |r| r.get::<_, String>(0))?
            .map(|row| Ok(crate::timing::snapshot(serde_json::from_str(&row?)?)))
            .collect()
    }
    pub fn page(
        &self,
        limit: usize,
        offset: usize,
        client: Option<&str>,
        lane: Option<&str>,
    ) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let filter = " FROM jobs j JOIN job_index i ON j.id=i.id WHERE (?1 IS NULL OR i.client_id=?1) AND (?2 IS NULL OR i.workload=?2)";
        let total: usize = db.query_row(
            &format!("SELECT COUNT(*){filter}"),
            params![client, lane],
            |r| r.get(0),
        )?;
        let mut stmt = db.prepare(&format!(
            "SELECT j.payload{filter} ORDER BY i.created_at DESC,i.id DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let jobs: Vec<Value> = stmt
            .query_map(params![client, lane, limit, offset], |r| {
                r.get::<_, String>(0)
            })?
            .map(|r| Ok(crate::timing::snapshot(serde_json::from_str(&r?)?)))
            .collect::<Result<_>>()?;
        Ok(
            json!({"nextOffset":if offset+jobs.len()<total{Some(offset+jobs.len())}else{None},"jobs":jobs,"total":total,"offset":offset}),
        )
    }
    pub fn active_jobs(&self) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(concat!(
            "SELECT j.payload FROM jobs j JOIN job_index i ON j.id=i.id ",
            "WHERE i.status IN('uploading','queued','running')",
        ))?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(crate::timing::snapshot(serde_json::from_str(&r?)?)))
            .collect()
    }
    pub fn record_stage(&self, id: &str, stage: Value) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        let measurements = &mut job["metadata"]["measurements"];
        if !measurements.is_object() {
            *measurements = json!({"schemaVersion":1,"stages":[]});
        }
        let rows = measurements["stages"]
            .as_array_mut()
            .context("工程記録が不正です")?;
        if let Some(row) = rows.iter_mut().find(|row| row["id"] == stage["id"]) {
            *row = stage;
        } else {
            rows.push(stage);
        }
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(())
    }
    pub fn update(&self, id: &str, changes: Value) -> Result<Value> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        let was_active = active(&job);
        if !active(&job) && changes.get("status").is_some_and(|v| v != &job["status"]) {
            return Ok(job);
        }
        if !active(&job)
            && ["phase", "processedUntil", "message"]
                .iter()
                .any(|k| changes.get(k).is_some())
        {
            return Ok(job);
        }
        for (key, value) in changes.as_object().context("変更値が不正です")? {
            if key == "metadata" {
                for (k, v) in value.as_object().context("metadataが不正です")? {
                    job["metadata"][k] = v.clone()
                }
            } else {
                job[key] = value.clone()
            }
        }
        job["metadata"]["processingStatus"] = job["status"].clone();
        if was_active && !active(&job) {
            Self::publish_attempt(&tx, &mut job)?;
            crate::timing::finish(&mut job);
        }
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(crate::timing::snapshot(job))
    }
    pub fn append(&self, id: &str, segment: Value) -> Result<()> {
        self.append_many(id, vec![segment])
    }
    fn publish_attempt(db: &Connection, job: &mut Value) -> Result<()> {
        if job["metadata"]["gpuAttemptProvisional"] != true {
            return Ok(());
        }
        let id = job["id"].as_str().context("ジョブIDがありません")?;
        db.execute("INSERT INTO segments SELECT job_id,id,payload FROM gpu_attempt_segments WHERE job_id=?", [id])?;
        db.execute("DELETE FROM gpu_attempt_segments WHERE job_id=?", [id])?;
        job["segmentCount"] = job["metadata"]["gpuAttemptSegmentCount"].clone();
        job["metadata"]["gpuAttemptProvisional"] = false.into();
        Ok(())
    }
    pub fn begin_gpu_attempt(&self, id: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        if !active(&job) {
            return Ok(false);
        }
        if job["segmentCount"].as_u64().unwrap_or(0) != 0 {
            bail!("GPU試行前に確定字幕があります")
        }
        job["metadata"]["gpuAttemptProvisional"] = true.into();
        job["metadata"]["gpuAttemptSegmentCount"] = 0.into();
        job["metadata"]["gpuAttemptStarted"] = true.into();
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn retry_gpu_on_cpu(&self, id: &str, reason: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        if !active(&job) || job["metadata"]["gpuAttemptProvisional"] != true {
            return Ok(false);
        }
        tx.execute("DELETE FROM gpu_attempt_segments WHERE job_id=?", [id])?;
        let measured = crate::timing::snapshot(job.clone());
        job["metadata"]["gpuAttemptSeconds"] = measured["metadata"]["runSeconds"].clone();
        let reason = reason.chars().take(2000).collect::<String>();
        let fallback = json!({"gpuAttemptProvisional":false,"gpuAttemptSegmentCount":0,
            "executionDevice":"cpu","actualDevice":null,"gpuFallback":true,
            "gpuFallbackAt":now(),"gpuFallbackReason":reason,"modelReused":false,"detectedLanguages":null});
        for (key, value) in fallback.as_object().unwrap() {
            job["metadata"][key] = value.clone();
        }
        job["processedUntil"] = job["rangeStart"].as_f64().unwrap_or(0.).into();
        job["phase"] = "preparing".into();
        job["message"] = "GPUで実行できなかったためCPUへ切り替えています。".into();
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn has_speakers(&self, id: &str) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(concat!(
            "SELECT payload FROM segments WHERE job_id=?1 ",
            "UNION ALL SELECT payload FROM gpu_attempt_segments WHERE job_id=?1",
        ))?;
        for row in stmt.query_map([id], |r| r.get::<_, String>(0))? {
            let row: Value = serde_json::from_str(&row?)?;
            if row["speaker"].as_str().is_some_and(|s| !s.is_empty()) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// 複数字幕を1トランザクションで保存する。保存内容はappendを順に呼んだ場合と同じ。
    pub fn append_many(&self, id: &str, segments: Vec<Value>) -> Result<()> {
        if segments.is_empty() {
            return Ok(());
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        if !active(&job) {
            return Ok(());
        }
        let provisional = job["metadata"]["gpuAttemptProvisional"] == true;
        let mut count = if provisional {
            job["metadata"]["gpuAttemptSegmentCount"].as_u64()
        } else {
            job["segmentCount"].as_u64()
        }
        .unwrap_or(0);
        let mut until = job["processedUntil"].as_f64().unwrap_or(0.);
        {
            let mut insert = tx.prepare_cached(if provisional {
                "INSERT INTO gpu_attempt_segments VALUES(?,?,?)"
            } else {
                "INSERT INTO segments VALUES(?,?,?)"
            })?;
            for mut segment in segments {
                count += 1;
                segment["id"] = count.into();
                until = until.max(segment["end"].as_f64().context("字幕時刻が不正です")?);
                insert.execute(params![id, count, serde_json::to_string(&segment)?])?;
            }
        }
        if provisional {
            job["metadata"]["gpuAttemptSegmentCount"] = count.into();
        } else {
            job["segmentCount"] = count.into();
        }
        job["processedUntil"] = until.into();
        job["phase"] = if job["task"] == "diarization" {
            "diarizing"
        } else {
            "transcribing"
        }
        .into();
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(())
    }
    pub fn segments(&self, id: &str, after: u64, limit: usize) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let job = Self::get_db(&db, id)?;
        let mut stmt =
            db.prepare("SELECT payload FROM segments WHERE job_id=? AND id>? ORDER BY id LIMIT ?")?;
        let segments: Vec<Value> = stmt
            .query_map(params![id, after, limit], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect::<Result<_>>()?;
        let next = segments
            .last()
            .map(|s| s["id"].as_u64().unwrap())
            .unwrap_or(after);
        Ok(
            json!({"next":next,"segments":segments,"total":job["segmentCount"],
            "revision":job["revision"],"resultGeneration":job["metadata"]["resultGeneration"].as_u64().unwrap_or(0)}),
        )
    }
    pub fn edit(&self, id: &str, edits: &[Value]) -> Result<Vec<Value>> {
        if edits.is_empty() || edits.len() > 1000 {
            bail!("編集件数が不正です")
        };
        let mut seen = std::collections::HashSet::new();
        for edit in edits {
            let sid = edit["id"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("字幕IDが不正です")?;
            if !seen.insert(sid)
                || edit
                    .as_object()
                    .context("編集値が不正です")?
                    .keys()
                    .any(|k| !matches!(k.as_str(), "id" | "text" | "speaker"))
            {
                bail!("字幕IDが重複/編集値が不正です")
            }
            for (k, max) in [("text", 200000), ("speaker", 300)] {
                if let Some(v) = edit.get(k)
                    && v.as_str().is_none_or(|s| s.chars().count() > max)
                {
                    bail!("編集値が長すぎるか不正です")
                }
            }
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        let mut rows = vec![];
        for edit in edits {
            let raw: String = tx
                .query_row(
                    "SELECT payload FROM segments WHERE job_id=? AND id=?",
                    params![id, edit["id"].as_u64()],
                    |r| r.get(0),
                )
                .map_err(|_| anyhow::anyhow!("NOT_FOUND"))?;
            let mut row: Value = serde_json::from_str(&raw)?;
            for k in ["text", "speaker"] {
                if let Some(v) = edit.get(k) {
                    row[k] = v.clone()
                }
            }
            if edit.get("speaker").is_some() {
                row["speakerSource"] = "manual".into();
                row["speakerNeedsReview"] = false.into()
            }
            row["editedAt"] = now().into();
            rows.push(row);
        }
        for row in &rows {
            tx.execute(
                "UPDATE segments SET payload=? WHERE job_id=? AND id=?",
                params![serde_json::to_string(row)?, id, row["id"].as_u64()],
            )?;
        }
        Self::put(&tx, &mut job)?;
        tx.commit()?;
        Ok(rows)
    }
    pub fn delete_job(&self, id: &str, client_id: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let job = Self::get_db(&tx, id)?;
        if !matches!(
            job["status"].as_str(),
            Some("complete" | "error" | "cancelled" | "interrupted")
        ) {
            bail!("CONFLICT: 処理中・待機中のジョブは削除できません。")
        }
        if client_id.is_empty() || job["clientId"].as_str() != Some(client_id) {
            bail!("FORBIDDEN: 利用アプリが一致しません。")
        }
        let path = self.dir.join(id);
        // Check every path before any irreversible file operation. Never follow links.
        let exists = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error)
                    .context("DELETE_FAILED: 保存先を確認できません。履歴は保持しています。");
            }
        };
        if exists {
            let root = self
                .dir
                .canonicalize()
                .context("DELETE_FAILED: 保存先を確認できません。")?;
            let canonical = path
                .canonicalize()
                .context("DELETE_FAILED: 対象の保存先を確認できません。")?;
            if canonical.parent() != Some(root.as_path())
                || canonical.file_name() != Some(std::ffi::OsStr::new(id))
            {
                bail!("DELETE_FAILED: 削除対象の保存先を確認できません。")
            }
            validate_delete_tree(&path)?;
        }
        // Keep all files until the SQLite commit succeeds. Startup resolves interrupted moves.
        for statement in [
            "DELETE FROM gpu_attempt_segments WHERE job_id=?",
            "DELETE FROM restart_segments WHERE job_id=?",
            "DELETE FROM restart_attempts WHERE job_id=?",
            "DELETE FROM segments WHERE job_id=?",
            "DELETE FROM job_index WHERE id=?",
            "DELETE FROM jobs WHERE id=?",
        ] {
            tx.execute(statement, [id])
                .context("DELETE_FAILED: 記録の削除を確定できません。")?;
        }
        let trash = self
            .dir
            .join(format!(".deleted-{id}-{}", uuid::Uuid::new_v4().simple()));
        if exists {
            fs::rename(&path, &trash)
                .context("DELETE_FAILED: 保存ファイルを退避できません。履歴は保持しています。")?;
        }
        if let Err(error) = tx.commit() {
            if exists {
                fs::rename(&trash, &path).context(
                    "DELETE_FAILED: 履歴削除失敗後の媒体復元に失敗しました。起動時に再試行します。",
                )?;
            }
            return Err(error).context(
                "DELETE_FAILED: 記録の削除を確定できません。媒体と履歴は保持しています。",
            );
        }
        if exists {
            let _ = fs::remove_dir_all(trash);
        } // Retry cleanup on next startup.
        Ok(())
    }
    pub fn remove_media(&self, id: &str) -> Result<Value> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        if active(&job) {
            bail!("処理中の録音は整理できません")
        };
        let path = self.dir.join(id).join("source.media");
        let trash = self.dir.join(id).join("source.removing");
        if trash.exists() {
            bail!("媒体の整理が未完了です。サーバーを再起動して回復してください。")
        }
        let exists = path.exists();
        if exists {
            regular_job_file(&self.dir, id, "source.media")?;
            validate_delete_tree(&path)?;
            fs::rename(&path, &trash)?;
        }
        job["metadata"]["mediaRemoved"] = true.into();
        job["metadata"]["mediaRemovedAt"] = now().into();
        let saved = Self::put(&tx, &mut job).and_then(|_| tx.commit().map_err(Into::into));
        if let Err(error) = saved {
            if exists {
                fs::rename(&trash, &path)?;
            }
            return Err(error);
        }
        if exists {
            let _ = fs::remove_file(trash);
        }
        Ok(crate::timing::snapshot(job))
    }
    /// Replace only the new upload's pathname with another link to identical, immutable bytes.
    pub fn register_media(&self, id: &str, sha: &str, bytes: u64) -> Result<Value> {
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) || bytes == 0 {
            bail!("媒体ハッシュが不正です")
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut job = Self::get_db(&tx, id)?;
        if !active(&job) {
            bail!("アップロード中に処理を中止しました")
        }
        let path = regular_job_file(&self.dir, id, "source.media")?;
        if fs::metadata(&path)?.len() != bytes {
            bail!("媒体の容量が一致しません")
        }
        let candidates = {
            let mut stmt = tx.prepare(concat!(
                "SELECT id FROM jobs WHERE id<>? AND json_extract(payload,'$.sourceSha256')=? ",
                "AND json_extract(payload,'$.sourceBytes')=?",
            ))?;
            stmt.query_map(params![id, sha, bytes], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let original = self.dir.join(id).join("source.unshared");
        if original.exists() {
            bail!("媒体の保存処理が未完了です。サーバーを再起動して回復してください。")
        }
        let mut shared = false;
        for other in candidates {
            let Ok(candidate) = regular_job_file(&self.dir, &other, "source.media") else {
                continue;
            };
            if fs::metadata(&candidate)?.len() != bytes {
                continue;
            }
            fs::rename(&path, &original)?;
            if fs::hard_link(&candidate, &path).is_ok() {
                shared = true;
                break;
            }
            fs::rename(&original, &path)?; // Unsupported filesystem or a concurrent reader: keep the upload.
            break;
        }
        job["sourceSha256"] = sha.into();
        job["sourceBytes"] = bytes.into();
        job["metadata"]["mediaDeduplicationApplied"] = shared.into();
        job["metadata"]["mediaUploaded"] = true.into();
        job["metadata"]["mediaUploadComplete"] = true.into();
        job["restartReplay"] = json!({"schemaVersion":1,"metadata":job["metadata"]});
        let saved = Self::put(&tx, &mut job).and_then(|_| tx.commit().map_err(Into::into));
        if let Err(error) = saved {
            if shared {
                fs::remove_file(&path)?;
                fs::rename(&original, &path)?;
            }
            return Err(error);
        }
        if shared {
            let _ = fs::remove_file(original);
        }
        Ok(crate::timing::snapshot(job))
    }
    /// Offline only: reuse the normal store lock, paths and crash-recovery filenames.
    pub fn compact_media(&self, apply: bool) -> Result<Value> {
        use sha2::{Digest, Sha256};
        use std::{collections::HashMap, io::Read};
        let jobs: Vec<Value> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db.prepare("SELECT payload FROM jobs ORDER BY id")?;
            let raw = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            raw.into_iter()
                .map(|row| serde_json::from_str(&row))
                .collect::<std::result::Result<_, _>>()?
        };
        if jobs.iter().any(active) {
            bail!("処理・待機・転送中は整理できません")
        }
        let mut known = HashMap::<(String, u64), PathBuf>::new();
        let (mut scanned, mut duplicates, mut shared, mut indexed) = (0, 0, 0, 0);
        let mut logical_bytes = 0u64;
        for mut job in jobs {
            let id = job["id"]
                .as_str()
                .context("ジョブIDがありません")?
                .to_owned();
            if job["metadata"]["mediaRemoved"] == true {
                continue;
            }
            let path = self.dir.join(&id).join("source.media");
            if !path.try_exists()? {
                continue;
            }
            let path = regular_job_file(&self.dir, &id, "source.media")?;
            let before = fs::metadata(&path)?;
            if before.len() == 0 {
                continue;
            }
            let mut file = File::open(&path)?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            drop(file);
            let after = fs::metadata(&path)?;
            if before.len() != after.len() || before.modified()? != after.modified()? {
                bail!("整理中に媒体が変更されました。再点検してください")
            }
            let sha = hex::encode(hash.finalize());
            if job["sourceSha256"].as_str().is_some_and(|s| s != sha)
                || job["sourceBytes"]
                    .as_u64()
                    .is_some_and(|s| s != before.len())
            {
                bail!("記録済みの媒体指紋と一致しません。元データは削除しません")
            }
            scanned += 1;
            logical_bytes += before.len();
            let key = (sha.clone(), before.len());
            let candidate = known.get(&key);
            if candidate.is_some() {
                duplicates += 1;
            }
            let missing = job["sourceSha256"].is_null() || job["sourceBytes"].is_null();
            if apply {
                let backup = self.dir.join(&id).join("source.unshared");
                if backup.exists() {
                    bail!("媒体の退避ファイルが残っています。再起動して回復してください")
                }
                let mut linked = false;
                if let Some(candidate) = candidate {
                    fs::rename(&path, &backup)?;
                    if fs::hard_link(candidate, &path).is_ok() {
                        linked = true;
                    } else {
                        fs::rename(&backup, &path)?;
                    }
                }
                job["sourceSha256"] = sha.into();
                job["sourceBytes"] = before.len().into();
                if missing {
                    job["metadata"]["mediaDigestBackfilled"] = true.into();
                }
                if linked {
                    job["metadata"]["mediaDeduplicationApplied"] = true.into();
                }
                let saved = (|| -> Result<()> {
                    let mut db = self.db.lock().unwrap();
                    let tx = db.transaction()?;
                    Self::put(&tx, &mut job)?;
                    tx.commit()?;
                    Ok(())
                })();
                if let Err(error) = saved {
                    if linked {
                        fs::remove_file(&path)?;
                        fs::rename(&backup, &path)?;
                    }
                    return Err(error);
                }
                if linked {
                    fs::remove_file(backup)?;
                    shared += 1;
                }
                if missing {
                    indexed += 1;
                }
            }
            known.entry(key).or_insert(path);
        }
        Ok(
            json!({"applied":apply,"scannedFiles":scanned,"duplicatePaths":duplicates,"sharedPaths":shared,"backfilledJobs":indexed,"logicalBytes":logical_bytes,"note":"ハードリンク共用後もパスと論理容量は同じです。sharedPathsは操作件数で、物理削減量ではありません。"}),
        )
    }

    fn recover_media_operations(&self) -> Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(tail) = name.strip_prefix(".deleted-") {
                let Some((id, nonce)) = tail.split_once('-') else {
                    continue;
                };
                if !id_valid(id) || !id_valid(nonce) {
                    continue;
                }
                let path = entry.path();
                let db = self.db.lock().unwrap();
                let exists: bool =
                    db.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?)", [id], |r| {
                        r.get(0)
                    })?;
                if exists {
                    validate_delete_tree(&path)?;
                    let destination = self.dir.join(id);
                    if destination.exists() {
                        bail!("媒体の復元先が既に存在します。退避データは保持しました。")
                    }
                    fs::rename(path, destination)?;
                } else if validate_delete_tree(&path).is_ok() {
                    let _ = fs::remove_dir_all(path);
                }
            }
        }
        for job in self.list(None, None)? {
            let id = job["id"].as_str().context("ジョブIDがありません")?;
            for name in ["source.unshared", "source.removing"] {
                let pending = self.dir.join(id).join(name);
                if !pending.exists() {
                    continue;
                }
                regular_job_file(&self.dir, id, name)?;
                let source = self.dir.join(id).join("source.media");
                if job["metadata"]["mediaRemoved"] == true || source.exists() {
                    let _ = fs::remove_file(pending);
                } else {
                    fs::rename(pending, source)?;
                }
            }
        }
        Ok(())
    }
}
fn regular_job_file(root: &Path, id: &str, name: &str) -> Result<PathBuf> {
    if !id_valid(id) {
        bail!("ジョブIDが不正です")
    }
    let dir = root.join(id);
    for path in [&dir, &dir.join(name)] {
        let meta = fs::symlink_metadata(path)?;
        #[cfg(windows)]
        let reparse = {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse = false;
        if meta.file_type().is_symlink() || reparse {
            bail!("リンクされた媒体パスは変更しません")
        }
    }
    let file = dir.join(name);
    if !fs::metadata(&file)?.is_file()
        || dir.canonicalize()?.parent() != Some(root.canonicalize()?.as_path())
    {
        bail!("媒体の保存先が不正です")
    }
    Ok(file)
}
fn validate_delete_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).context("DELETE_FAILED: 削除対象を点検できません。")?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = meta.file_type().is_symlink();
    if reparse || (!meta.is_dir() && !meta.is_file()) {
        bail!("DELETE_FAILED: リンクや特殊ファイルは削除対象にできません。")
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // DELETE access detects an existing handle that denies deletion before removing files.
        let _guard = OpenOptions::new()
            .access_mode(0x10000)
            .share_mode(7)
            .custom_flags(if meta.is_dir() { 0x02000000 } else { 0 })
            .open(path)
            .context(
                "DELETE_FAILED: ファイルが使用中か削除権限がありません。履歴は保持しています。",
            )?;
    }
    if meta.is_dir() {
        for entry in
            fs::read_dir(path).context("DELETE_FAILED: 保存ファイル一覧を確認できません。")?
        {
            validate_delete_tree(&entry?.path())?;
        }
    }
    Ok(())
}
pub fn tree_size(path: &Path) -> Result<u64> {
    let mut size = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.file_type()?;
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            size += tree_size(&entry.path())?
        } else {
            size += entry.metadata()?.len()
        }
    }
    Ok(size)
}
#[cfg(test)]
mod tests {
    fn replay_job(store: &Store) -> String {
        use sha2::{Digest, Sha256};
        let job=store.create(json!({"engine":"whisper","modelId":"tiny","device":"cpu","workload":"file","status":"uploading","segmentCount":0,"rangeStart":0.,"processedUntil":0.,"metadata":{"executionDevice":"cpu","timingVersion":1,"recognitionOptions":{"language":"ja","prompt":"TranscriptCutter"}}})).unwrap();
        let id = job["id"].as_str().unwrap();
        let media = b"restart-fixture";
        fs::write(store.dir.join(id).join("source.media"), media).unwrap();
        store
            .register_media(id, &hex::encode(Sha256::digest(media)), media.len() as u64)
            .unwrap();
        id.into()
    }
    #[test]
    fn restart_replays_same_id_atomically_resets_subtitles_and_preserves_terminal_jobs() {
        let (root, store) = fixture();
        let id = replay_job(&store);
        let complete = replay_job(&store);
        store
            .update(&complete, json!({"status":"complete"}))
            .unwrap();
        let cancelled = replay_job(&store);
        store
            .update(&cancelled, json!({"status":"cancelled"}))
            .unwrap();
        store
            .update(
                &id,
                json!({"status":"running","metadata":{"startedAt":now()}}),
            )
            .unwrap();
        store
            .append(&id, json!({"text":"old partial","start":0.,"end":1.}))
            .unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        let job = store.get(&id).unwrap();
        assert_eq!(job["id"], id);
        assert_eq!(job["status"], "queued");
        assert_eq!(job["metadata"]["resultGeneration"], 1);
        assert_eq!(
            job["metadata"]["recognitionOptions"],
            json!({"language":"ja","prompt":"TranscriptCutter"})
        );
        assert_eq!(job["metadata"]["runSeconds"], 0.);
        assert_eq!(store.segments(&id, 0, 100).unwrap()["total"], 0);
        assert_eq!(store.get(&complete).unwrap()["status"], "complete");
        assert_eq!(store.get(&cancelled).unwrap()["status"], "cancelled");
        store
            .append(&id, json!({"text":"fresh","start":0.,"end":1.}))
            .unwrap();
        let rows = store.segments(&id, 0, 100).unwrap();
        assert_eq!(rows["segments"].as_array().unwrap().len(), 1);
        assert_eq!(rows["segments"][0]["id"], 1);
        assert_eq!(rows["resultGeneration"], 1);
        let archived: i64 = store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM restart_segments WHERE job_id=?",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived, 1);
        store.update(&id, json!({"status":"complete"})).unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.get(&id).unwrap()["metadata"]["resultGeneration"], 1);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restart_refuses_incomplete_corrupt_external_and_repeated_failure() {
        let (root, store) = fixture();
        let id = replay_job(&store);
        let corrupt = replay_job(&store);
        fs::remove_file(store.dir.join(&corrupt).join("source.media")).unwrap();
        fs::write(
            store.dir.join(&corrupt).join("source.media"),
            b"changed-fixture",
        )
        .unwrap();
        let external = replay_job(&store);
        store
            .update(
                &external,
                json!({"engine":"openai","metadata":{"externalApi":true}}),
            )
            .unwrap();
        let incomplete = store
            .create(json!({"status":"uploading","engine":"whisper","metadata":{}}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        drop(store);
        for count in 1..=3 {
            let store = Store::open(&root).unwrap();
            let job = store.get(&id).unwrap();
            assert_eq!(job["status"], "queued");
            assert_eq!(job["metadata"]["recoveryCount"], count);
            assert_eq!(store.get(&corrupt).unwrap()["status"], "interrupted");
            assert_eq!(store.get(&external).unwrap()["status"], "interrupted");
            assert_eq!(store.get(&incomplete).unwrap()["status"], "interrupted");
        }
        let store = Store::open(&root).unwrap();
        assert_eq!(store.get(&id).unwrap()["status"], "interrupted");
        assert!(
            store.get(&id).unwrap()["metadata"]["recoveryBlockedReason"]
                .as_str()
                .unwrap()
                .contains("上限")
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    #[test]
    fn offline_compaction_backfills_legacy_media_and_future_uploads_reuse_it() {
        let (root, store) = fixture();
        let create = |bytes: &[u8], status: &str| {
            let job = store
                .create(json!({"status":status,"clientId":"test","metadata":{}}))
                .unwrap();
            let id = job["id"].as_str().unwrap().to_owned();
            fs::write(root.join(&id).join("source.media"), bytes).unwrap();
            id
        };
        let a = create(b"legacy immutable video", "completed");
        let b = create(b"legacy immutable video", "completed");
        let c = create(b"different immutable video", "completed");
        let plan = store.compact_media(false).unwrap();
        assert_eq!(plan["duplicatePaths"], 1);
        assert!(store.get(&a).unwrap()["sourceSha256"].is_null());
        let result = store.compact_media(true).unwrap();
        assert_eq!(result["backfilledJobs"], 3);
        assert_eq!(result["sharedPaths"], 1);
        assert_eq!(store.get(&a).unwrap()["status"], "completed");
        assert_eq!(store.list(None, None).unwrap().len(), 3);
        let sha = store.get(&a).unwrap()["sourceSha256"]
            .as_str()
            .unwrap()
            .to_owned();
        let next = create(b"legacy immutable video", "uploading");
        store.register_media(&next, &sha, 22).unwrap();
        assert_eq!(
            store.get(&next).unwrap()["metadata"]["mediaDeduplicationApplied"],
            true
        );
        assert!(store.compact_media(true).is_err());
        fs::write(
            root.join(&a).join("source.media"),
            b"fixture-only identity check",
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join(&b).join("source.media")).unwrap(),
            b"fixture-only identity check"
        );
        assert_eq!(
            fs::read(root.join(&c).join("source.media")).unwrap(),
            b"different immutable video"
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_compaction_rejects_changed_digest_and_restores_files_on_db_failure() {
        let (root, store) = fixture();
        for id in ["a".repeat(32), "b".repeat(32)] {
            fs::create_dir(root.join(&id)).unwrap();
            fs::write(root.join(&id).join("source.media"), b"same").unwrap();
            let mut job = json!({"id":id,"createdAt":"fixture","status":"completed","clientId":"test","workload":"file","metadata":{}});
            Store::put(&store.db.lock().unwrap(), &mut job).unwrap();
        }
        store.db.lock().unwrap().execute_batch("CREATE TRIGGER fail_compact BEFORE INSERT ON jobs WHEN new.id='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(store.compact_media(true).is_err());
        assert_eq!(
            fs::read(root.join("b".repeat(32)).join("source.media")).unwrap(),
            b"same"
        );
        assert!(!root.join("b".repeat(32)).join("source.unshared").exists());
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_compact;")
            .unwrap();
        fs::write(root.join("a".repeat(32)).join("source.media"), b"changed").unwrap();
        assert!(store.compact_media(true).is_err());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn identical_uploads_share_bytes_and_release_only_the_selected_reference() {
        use sha2::{Digest, Sha256};
        use std::sync::Arc;
        let (root, store) = fixture();
        let store = Arc::new(store);
        let bytes = b"synthetic immutable media";
        let sha = hex::encode(Sha256::digest(bytes));
        let create = || {
            let j = store
                .create(json!({"status":"uploading","clientId":"test","metadata":{}}))
                .unwrap();
            let id = j["id"].as_str().unwrap().to_owned();
            fs::write(root.join(&id).join("source.media"), bytes).unwrap();
            id
        };
        let a = create();
        let b = create();
        let c = create();
        store.register_media(&a, &sha, bytes.len() as u64).unwrap();
        let failed = create();
        store.db.lock().unwrap().execute_batch(&format!("CREATE TRIGGER reject_media_put BEFORE INSERT ON jobs WHEN new.id='{failed}' BEGIN SELECT RAISE(ABORT,'fixture media write failure'); END;")).unwrap();
        assert!(
            store
                .register_media(&failed, &sha, bytes.len() as u64)
                .is_err()
        );
        assert_eq!(
            fs::read(root.join(&failed).join("source.media")).unwrap(),
            bytes
        );
        assert!(!root.join(&failed).join("source.unshared").exists());
        assert!(store.get(&failed).unwrap()["sourceSha256"].is_null());
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_media_put;")
            .unwrap();
        std::thread::scope(|scope| {
            for id in [&b, &c] {
                let store = &store;
                let sha = &sha;
                scope.spawn(move || {
                    store.register_media(id, sha, bytes.len() as u64).unwrap();
                });
            }
        });
        assert_eq!(store.get(&b).unwrap()["sourceSha256"], sha);
        assert_eq!(store.get(&b).unwrap()["sourceBytes"], bytes.len());
        assert_eq!(
            store.get(&c).unwrap()["metadata"]["mediaDeduplicationApplied"],
            true
        );
        // Fixture-only mutation proves these are the same filesystem object, not copies.
        fs::write(root.join(&a).join("source.media"), b"changed").unwrap();
        assert_eq!(
            fs::read(root.join(&b).join("source.media")).unwrap(),
            b"changed"
        );
        assert_eq!(
            fs::read(root.join(&c).join("source.media")).unwrap(),
            b"changed"
        );
        fs::write(root.join(&a).join("source.media"), bytes).unwrap();
        for id in [&a, &b, &c] {
            store.update(id, json!({"status":"complete"})).unwrap();
        }
        store.delete_job(&a, "test").unwrap();
        store.remove_media(&b).unwrap();
        assert_eq!(fs::read(root.join(&c).join("source.media")).unwrap(), bytes);
        assert_eq!(store.get(&b).unwrap()["sourceSha256"], sha);
        store.delete_job(&c, "test").unwrap();
        assert!(!root.join(&a).exists());
        assert!(!root.join(&b).join("source.media").exists());
        assert!(!root.join(&c).exists());
        let legacy = store.create(job()).unwrap();
        assert!(legacy["sourceSha256"].is_null());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn deletion_commit_failure_and_interrupted_file_operations_preserve_media() {
        let (root, store) = fixture();
        let record = store
            .create(json!({"status":"complete","clientId":"test","metadata":{}}))
            .unwrap();
        let id = record["id"].as_str().unwrap();
        let media = root.join(id).join("source.media");
        fs::write(&media, b"preserve").unwrap();
        // Failure at COMMIT, after the directory has moved, must restore every file.
        store.db.lock().unwrap().execute_batch(&format!("PRAGMA foreign_keys=ON; CREATE TABLE hold_reference(job_id TEXT REFERENCES jobs(id) DEFERRABLE INITIALLY DEFERRED); INSERT INTO hold_reference VALUES('{id}');")).unwrap();
        assert!(store.delete_job(id, "test").is_err());
        assert_eq!(fs::read(&media).unwrap(), b"preserve");
        assert!(store.get(id).is_ok());
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE hold_reference; PRAGMA foreign_keys=OFF;")
            .unwrap();
        store.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_put BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT,'fixture write failure'); END;").unwrap();
        assert!(store.remove_media(id).is_err());
        assert_eq!(fs::read(&media).unwrap(), b"preserve");
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_put;")
            .unwrap();
        let trash = root.join(format!(".deleted-{id}-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(root.join(id), &trash).unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(fs::read(&media).unwrap(), b"preserve");
        assert!(!trash.exists());
        fs::rename(&media, root.join(id).join("source.removing")).unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(fs::read(&media).unwrap(), b"preserve");
        let trash = root.join(format!(".deleted-{id}-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(root.join(id), &trash).unwrap();
        store
            .db
            .lock()
            .unwrap()
            .execute("DELETE FROM jobs WHERE id=?", [id])
            .unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        assert!(!trash.exists());
        assert!(store.get(id).is_err());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    fn fixture() -> (PathBuf, Store) {
        let path = std::env::temp_dir().join(format!("tc-rust-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&path).unwrap();
        (path, store)
    }
    fn job() -> Value {
        json!({"status":"queued","phase":"queued","segmentCount":0,"processedUntil":0,"metadata":{}})
    }
    #[test]
    fn delete_job_sql_failure_rolls_back_all_rows_before_any_media_deletion() {
        let (root, store) = fixture();
        let job=store.create(json!({"status":"running","clientId":"transcript-cutter","segmentCount":0,"metadata":{}})).unwrap();
        let id = job["id"].as_str().unwrap();
        store
            .append(id, json!({"start":0.,"end":1.,"text":"synthetic"}))
            .unwrap();
        store.update(id, json!({"status":"complete"})).unwrap();
        fs::write(store.dir.join(id).join("source.media"), b"synthetic media").unwrap();
        store.db.lock().unwrap().execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON jobs BEGIN SELECT RAISE(ABORT,'fixture database failure'); END;").unwrap();
        assert!(
            store
                .delete_job(id, "transcript-cutter")
                .unwrap_err()
                .to_string()
                .starts_with("DELETE_FAILED:")
        );
        assert!(store.get(id).is_ok());
        assert_eq!(store.segments(id, 0, 10).unwrap()["total"], 1);
        assert_eq!(
            fs::read(store.dir.join(id).join("source.media")).unwrap(),
            b"synthetic media"
        );
        store
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_delete")
            .unwrap();
        store.delete_job(id, "transcript-cutter").unwrap();
        assert!(store.get(id).is_err());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn gpu_attempt_is_atomic_retry_discards_and_cancel_recovery_preserve_partials() {
        let (p, s) = fixture();
        let j = s.create(job()).unwrap();
        let id = j["id"].as_str().unwrap();
        s.begin_gpu_attempt(id).unwrap();
        s.append(id, json!({"text":"discard GPU","start":0,"end":1}))
            .unwrap();
        assert_eq!(s.segments(id, 0, 10).unwrap()["total"], 0);
        assert!(
            s.segments(id, 0, 10).unwrap()["segments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(s.retry_gpu_on_cpu(id, "GPU failure").unwrap());
        s.append(id, json!({"text":"CPU only","start":0,"end":1}))
            .unwrap();
        s.update(id, json!({"status":"complete"})).unwrap();
        assert_eq!(
            s.segments(id, 0, 10).unwrap()["segments"][0]["text"],
            "CPU only"
        );
        let j = s.create(job()).unwrap();
        let id = j["id"].as_str().unwrap();
        s.begin_gpu_attempt(id).unwrap();
        s.append(
            id,
            json!({"text":"kept partial","start":0,"end":1,"speaker":"s1"}),
        )
        .unwrap();
        assert!(s.has_speakers(id).unwrap());
        s.update(id, json!({"status":"cancelled"})).unwrap();
        assert!(!s.retry_gpu_on_cpu(id, "too late").unwrap());
        assert!(!s.begin_gpu_attempt(id).unwrap());
        assert_eq!(s.segments(id, 0, 10).unwrap()["total"], 1);
        let j = s.create(job()).unwrap();
        let id = j["id"].as_str().unwrap().to_owned();
        s.begin_gpu_attempt(&id).unwrap();
        s.append(&id, json!({"text":"recovered","start":0,"end":1}))
            .unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.get(&id).unwrap()["status"], "interrupted");
        assert_eq!(
            s.segments(&id, 0, 10).unwrap()["segments"][0]["text"],
            "recovered"
        );
        drop(s);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn atomic_edits_and_cancel_preserve_terminal_state() {
        let (p, s) = fixture();
        let j = s.create(job()).unwrap();
        let id = j["id"].as_str().unwrap();
        s.append(
            id,
            json!({"text":"original","start":0,"end":1,"speaker":"話者1"}),
        )
        .unwrap();
        assert!(
            s.edit(
                id,
                &[
                    json!({"id":1,"text":"bad"}),
                    json!({"id":999,"text":"absent"})
                ]
            )
            .is_err()
        );
        assert_eq!(
            s.segments(id, 0, 5).unwrap()["segments"][0]["text"],
            "original"
        );
        s.update(id, json!({"status":"cancelled"})).unwrap();
        s.update(id, json!({"status":"complete"})).unwrap();
        s.append(id, json!({"text":"late","end":2})).unwrap();
        assert_eq!(s.get(id).unwrap()["status"], "cancelled");
        assert_eq!(s.get(id).unwrap()["segmentCount"], 1);
        drop(s);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn append_many_stores_the_same_rows_as_sequential_appends() {
        let (p, s) = fixture();
        let rows = [
            json!({"text":"一","start":0.5,"end":2.0,"speaker":"話者1"}),
            json!({"text":"二","start":2.0,"end":1.5}),
            json!({"text":"三","start":3.0,"end":4.25,"speakerCandidates":[]}),
        ];
        let one = s.create(job()).unwrap();
        let many = s.create(job()).unwrap();
        let (a, b) = (one["id"].as_str().unwrap(), many["id"].as_str().unwrap());
        for row in &rows {
            s.append(a, row.clone()).unwrap();
        }
        s.append_many(b, rows.to_vec()).unwrap();
        s.append_many(b, vec![]).unwrap();
        assert_eq!(
            s.segments(a, 0, 10).unwrap()["segments"],
            s.segments(b, 0, 10).unwrap()["segments"]
        );
        let (ja, jb) = (s.get(a).unwrap(), s.get(b).unwrap());
        for key in ["segmentCount", "processedUntil", "phase"] {
            assert_eq!(ja[key], jb[key], "{key}");
        }
        assert_eq!(jb["processedUntil"], 4.25);
        s.update(b, json!({"status":"cancelled"})).unwrap();
        s.append_many(b, vec![json!({"text":"late","end":9})])
            .unwrap();
        assert_eq!(s.get(b).unwrap()["segmentCount"], 3);
        assert!(
            s.append_many(a, vec![json!({"text":"ok","end":5}), json!({"text":"bad"})])
                .is_err()
        );
        assert_eq!(s.get(a).unwrap()["segmentCount"], 3);
        drop(s);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn legacy_schema_and_unknown_fields_survive() {
        let (p, s) = fixture();
        let j=s.create(json!({"status":"complete","createdAt":"2025-01-01","metadata":{"oldExtra":"kept"},"unknown":{"x":1}})).unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.get(j["id"].as_str().unwrap()).unwrap()["unknown"]["x"], 1);
        assert_eq!(s.page(1, 0, None, None).unwrap()["total"], 1);
        assert!(Store::open(&p).is_err());
        drop(s);
        fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn final_times_are_persisted_without_read_heartbeats_and_recovery_excludes_offline_time() {
        let (p, s) = fixture();
        let j = s.create(job()).unwrap();
        let id = j["id"].as_str().unwrap();
        s.update(id, json!({"status":"running","metadata":{"queuedAt":"2026-01-01T00:00:00Z","dequeuedAt":"2026-01-01T00:00:02Z","startedAt":"2026-01-01T00:00:05Z"}})).unwrap();
        let before = s.get(id).unwrap();
        let read = s.page(10, 0, None, None).unwrap();
        assert_eq!(read["jobs"][0]["revision"], before["revision"]);
        let ended = s.update(id, json!({"status":"complete","metadata":{"completedAt":"2026-01-01T00:00:10Z","finishedAt":"2026-01-01T00:00:10Z"}})).unwrap();
        assert_eq!(ended["metadata"]["runSeconds"], 5.);
        assert_eq!(ended["metadata"]["waitSeconds"], 5.);
        let raw = Store::get_db(&s.db.lock().unwrap(), id).unwrap();
        assert_eq!(raw["metadata"]["runSeconds"], 5.);
        let pending = s.create(job()).unwrap();
        let pid = pending["id"].as_str().unwrap();
        s.update(pid, json!({"status":"running","metadata":{"queuedAt":"2026-01-01T00:00:00Z","dequeuedAt":"2026-01-01T00:00:02Z","startedAt":"2026-01-01T00:00:05Z"}})).unwrap();
        let last_update = s.get(pid).unwrap()["updatedAt"].clone();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.get(id).unwrap()["metadata"]["runSeconds"], 5.);
        let recovered = s.get(pid).unwrap();
        assert_eq!(recovered["status"], "interrupted");
        assert_eq!(recovered["metadata"]["finishedAt"], last_update);
        assert_eq!(recovered["metadata"]["timingEstimated"], true);
        drop(s);
        fs::remove_dir_all(p).unwrap();
    }
}
