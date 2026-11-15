use crate::{
    platform,
    store::{Store, now},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub struct Stage<'a> {
    store: &'a Store,
    job: &'a str,
    cancel: &'a CancellationToken,
    started: Instant,
    value: Value,
    done: bool,
}
impl<'a> Stage<'a> {
    pub fn new(
        store: &'a Store,
        job: &'a str,
        stage: &str,
        label: &str,
        cancel: &'a CancellationToken,
    ) -> Result<Self> {
        let mut value = json!({"id":uuid::Uuid::new_v4().simple().to_string(),"stage":stage,"label":label,
            "status":"running","startedAt":now(),"finishedAt":null,"elapsedSeconds":null,
            "clock":"server_monotonic","memory":{"scope":"owned_worker_process_tree","metric":"private_committed_bytes",
            "unit":"bytes","method":"Windows PrivateUsage sampled sum","peakKind":"sampled_maximum",
            "samplingIntervalMs":200,"sampleCount":0,"peakBytes":null,"lastBytes":null,"notMeasuredReason":"no_successful_sample"}});
        let snapshot = store.get(job)?;
        value["executionDevice"] = snapshot["metadata"]["executionDevice"].clone();
        value["cpuFallbackAttempt"] = (snapshot["metadata"]["gpuFallback"] == true).into();
        store.record_stage(job, value.clone())?;
        Ok(Self {
            store,
            job,
            cancel,
            started: Instant::now(),
            value,
            done: false,
        })
    }
    pub fn sample(&mut self, scope: &platform::Job) {
        if let Some(bytes) = scope.memory_bytes() {
            let memory = &mut self.value["memory"];
            memory["sampleCount"] = (memory["sampleCount"].as_u64().unwrap_or(0) + 1).into();
            memory["lastBytes"] = bytes.into();
            memory["peakBytes"] = memory["peakBytes"].as_u64().unwrap_or(0).max(bytes).into();
            memory["notMeasuredReason"] = Value::Null;
        }
    }
    pub fn finish(&mut self, status: &str) -> Result<()> {
        self.value["status"] = status.into();
        self.value["finishedAt"] = now().into();
        self.value["elapsedSeconds"] = self.started.elapsed().as_secs_f64().into();
        self.store.record_stage(self.job, self.value.clone())?;
        self.done = true;
        Ok(())
    }
}
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.finish(if self.cancel.is_cancelled() {
                "cancelled"
            } else {
                "interrupted"
            });
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn measure<T>(
    store: &Store,
    job: &str,
    kind: &str,
    label: &str,
    cancel: &CancellationToken,
    scope: Option<&platform::Job>,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut stage = Stage::new(store, job, kind, label, cancel)?;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tokio::pin!(future);
    let result = loop {
        tokio::select! {
            result=&mut future=>break result,
            _=tick.tick()=>if let Some(scope)=scope {stage.sample(scope)},
        }
    };
    stage.finish(if cancel.is_cancelled() {
        "cancelled"
    } else if result.is_ok() {
        "complete"
    } else {
        "error"
    })?;
    result
}

/// Keep one read future alive across sampler ticks: cancelling read_line loses partial UTF-8/JSON.
pub async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
    cancel: &CancellationToken,
    mut sample: impl FnMut(),
) -> Result<usize> {
    use tokio::io::AsyncBufReadExt;
    let read = reader.read_line(line);
    tokio::pin!(read);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("処理を中止しました"),
            _=tick.tick()=>sample(),
            count=&mut read=>return Ok(count?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn sampler_ticks_do_not_discard_partial_json_lines() {
        use tokio::io::{AsyncWriteExt, BufReader};
        let (mut writer, reader) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            writer.write_all(b"{\"type\":").await.unwrap();
            tokio::time::sleep(Duration::from_millis(450)).await;
            writer.write_all(b"\"done\"}\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let token = CancellationToken::new();
        let mut ticks = 0;
        let n = read_line(&mut reader, &mut line, &token, || ticks += 1)
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(n, line.len());
        assert!(ticks >= 2);
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["type"],
            "done"
        );
    }
    #[tokio::test]
    async fn parallel_error_cancel_and_reopen_preserve_distinct_stage_records() {
        let root =
            std::env::temp_dir().join(format!("transcription-stages-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&root).unwrap();
        let job = store
            .create(json!({"status":"running","metadata":{}}))
            .unwrap();
        let id = job["id"].as_str().unwrap();
        let token = CancellationToken::new();
        let a = measure(&store, id, "recognition", "認識", &token, None, async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(())
        });
        let b = measure(&store, id, "diarization", "話者分離", &token, None, async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Err::<(), _>(anyhow::anyhow!("fixture"))
        });
        let (a, b) = tokio::join!(a, b);
        assert!(a.is_ok() && b.is_err());
        let cancelled = token.child_token();
        let stage = Stage::new(&store, id, "cancelled", "中止", &cancelled).unwrap();
        cancelled.cancel();
        drop(stage);
        let mut unfinished = Stage::new(&store, id, "crash", "異常終了", &token).unwrap();
        unfinished.done = true;
        drop(unfinished);
        let rows = store.get(id).unwrap()["metadata"]["measurements"]["stages"].clone();
        assert_eq!(rows.as_array().unwrap().len(), 4);
        assert_eq!(rows[0]["status"], "complete");
        assert_eq!(rows[1]["status"], "error");
        assert_eq!(rows[2]["status"], "cancelled");
        assert!(rows[0]["elapsedSeconds"].as_f64().unwrap() >= 0.02);
        assert!(rows[0]["memory"]["peakBytes"].is_null());
        assert_eq!(rows[0]["memory"]["sampleCount"], 0);
        let id = id.to_owned();
        drop(store);
        let reopened = Store::open(&root).unwrap();
        let recovered = reopened.get(&id).unwrap();
        assert_eq!(recovered["metadata"]["measurements"]["stages"][0], rows[0]);
        assert_eq!(
            recovered["metadata"]["measurements"]["stages"][3]["status"],
            "interrupted"
        );
        assert!(recovered["metadata"]["measurements"]["stages"][3]["elapsedSeconds"].is_null());
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn memory_samples_only_its_owned_process_group() {
        use std::{path::Path, process::Stdio};
        use tokio::io::{AsyncBufReadExt, BufReader};
        let scope = platform::Job::new().unwrap();
        let unrelated = platform::Job::new().unwrap();
        let mut child=platform::command(Path::new("pwsh")).args(["-NoProfile","-NonInteractive","-Command","$block=New-Object byte[] 67108864; for($index=0;$index -lt $block.Length;$index+=4096){$block[$index]=1}; Write-Output ready; Start-Sleep -Seconds 30"]).stdout(Stdio::piped()).spawn().unwrap();
        scope.assign(&child).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(15), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line.trim(), "ready");
        assert!(scope.memory_bytes().unwrap() > 64 * 1024 * 1024);
        assert_eq!(unrelated.memory_bytes(), None);
        scope.terminate().unwrap();
        child.wait().await.unwrap();
    }
}
