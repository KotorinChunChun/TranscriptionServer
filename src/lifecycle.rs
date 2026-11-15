//! Drain closes admission, while the scheduler keeps accepted work running.
use crate::app::App;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Ticket {
    id: String,
    cancel: CancellationToken,
}
pub struct Drain<'a> {
    app: &'a App,
    ticket: Ticket,
    committed: bool,
}
async fn begin(app: &App, purpose: &str, updater: bool) -> Result<Ticket> {
    let _gate = app.admission.lock().await;
    if app.maintenance.load(Ordering::Acquire)
        || app.draining.load(Ordering::Acquire)
        || app.stop.is_cancelled()
        || (!updater && app.update_busy.load(Ordering::Acquire))
    {
        bail!("CONFLICT: 準備・停止・更新操作中です")
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let cancel = CancellationToken::new();
    *app.drain_cancel.lock().unwrap() = cancel.clone();
    *app.drain_state.lock().unwrap() = json!({"id":id,"state":"waiting","purpose":purpose,"message":"新規受付を停止し、受付済み処理の完了を待っています。"});
    app.draining.store(true, Ordering::Release);
    Ok(Ticket { id, cancel })
}
impl<'a> Drain<'a> {
    pub async fn begin(app: &'a App, purpose: &str, updater: bool) -> Result<Self> {
        Ok(Self {
            app,
            ticket: begin(app, purpose, updater).await?,
            committed: false,
        })
    }
    pub async fn wait(&mut self) -> Result<()> {
        loop {
            if self.ticket.cancel.is_cancelled() {
                bail!("完了待ちを取り消しました")
            }
            if self.app.stop.is_cancelled() {
                bail!("サーバーが停止しました")
            }
            {
                let _gate = self.app.admission.lock().await;
                if self.ticket.cancel.is_cancelled() {
                    bail!("完了待ちを取り消しました")
                }
                if self.app.incoming.load(Ordering::Acquire) == 0
                    && self.app.store.active_jobs()?.is_empty()
                    && self.app.scheduler.lock().unwrap().active.is_empty()
                    && self.app.running.lock().unwrap().is_empty()
                {
                    self.app.maintenance.store(true, Ordering::Release);
                    self.app.drain_state.lock().unwrap()["state"] = "ready".into();
                    return Ok(());
                }
            }
            tokio::select! {
                _=self.ticket.cancel.cancelled()=>bail!("完了待ちを取り消しました"),
                _=self.app.stop.cancelled()=>bail!("サーバーが停止しました"),
                _=tokio::time::sleep(Duration::from_millis(100))=>{}
            }
        }
    }
    pub fn commit(&mut self) {
        self.committed = true;
    }
    /// Atomically stop accepting cancellation before shutdown/update approval.
    pub async fn seal(&self) -> Result<()> {
        let _gate = self.app.admission.lock().await;
        if self.ticket.cancel.is_cancelled() || self.app.stop.is_cancelled() {
            bail!("完了待ちを取り消しました")
        }
        self.app.drain_state.lock().unwrap()["state"] = "stopping".into();
        Ok(())
    }
}
impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if !self.committed && self.app.drain_state.lock().unwrap()["id"] == self.ticket.id {
            self.app.drain_state.lock().unwrap()["state"] = "cancelled".into();
            self.app.draining.store(false, Ordering::Release);
            self.app.maintenance.store(false, Ordering::Release);
        }
    }
}
pub fn status(app: &App) -> Value {
    let mut value = app.drain_state.lock().unwrap().clone();
    value["active"] = app.draining.load(Ordering::Acquire).into();
    value["jobsRemaining"] = app.store.active_jobs().map(|j| j.len()).ok().into();
    value["uploadsRemaining"] = app.incoming.load(Ordering::Acquire).into();
    value
}
pub async fn cancel(app: &App, id: &str) -> Result<Value> {
    let _gate = app.admission.lock().await;
    let state = app.drain_state.lock().unwrap().clone();
    if state["id"] != id || !app.draining.load(Ordering::Acquire) {
        bail!("CONFLICT: 完了待ちの対象が一致しません")
    }
    if state["state"] == "stopping" {
        bail!("CONFLICT: 正常終了を開始済みです")
    }
    app.drain_cancel.lock().unwrap().cancel();
    if state["purpose"] == "hold" && state["state"] == "ready" {
        app.draining.store(false, Ordering::Release);
        app.maintenance.store(false, Ordering::Release);
        app.drain_state.lock().unwrap()["state"] = "cancelled".into();
    }
    Ok(json!({"accepted":true}))
}
pub async fn request(app: Arc<App>, purpose: &str) -> Result<Value> {
    if !matches!(purpose, "shutdown" | "restart" | "hold") {
        bail!("停止操作が不正です")
    }
    let ticket = begin(&app, purpose, false).await?;
    let id = ticket.id.clone();
    let purpose = purpose.to_string();
    tokio::spawn(async move {
        let mut drain = Drain {
            app: &app,
            ticket,
            committed: false,
        };
        if drain.wait().await.is_err() {
            return;
        }
        if purpose != "hold" {
            if drain.seal().await.is_err() {
                return;
            }
            app.restart_requested
                .store(purpose == "restart", Ordering::Release);
            tokio::time::sleep(Duration::from_millis(300)).await;
            app.stop.cancel();
        }
        drain.commit();
    });
    Ok(json!({"accepted":true,"id":id,"state":"waiting"}))
}
