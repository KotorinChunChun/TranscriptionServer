//! Installer helper: validate the installed image, then wait for normal shutdown.
use crate::{config, platform};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn same_image(actual: &Path, expected: &Path) -> Result<bool> {
    Ok(config::friendly(&std::fs::canonicalize(actual)?)
        .to_string_lossy()
        .eq_ignore_ascii_case(
            &config::friendly(&std::fs::canonicalize(expected)?).to_string_lossy(),
        ))
}
pub async fn run(args: &[String]) -> Result<()> {
    let install = PathBuf::from(args.get(2).context("導入先がありません")?);
    let report = PathBuf::from(args.get(3).context("報告先がありません")?);
    let cancel = report.with_extension("cancel");
    write_report(&report, "starting", "停止処理を開始しています。", None)?;
    let result = prepare(&install, &report, &cancel).await;
    let (state, message) = match &result {
        Ok(()) => ("complete", "対象サーバーの停止を確認しました。".into()),
        Err(e) => ("error", e.to_string()),
    };
    write_report(&report, state, &message, None)?;
    result
}
fn write_report(path: &Path, state: &str, message: &str, server_pid: Option<u32>) -> Result<()> {
    let text = path.with_extension("txt");
    let temp = path.with_extension("txt.tmp");
    std::fs::write(&temp, message)?;
    std::fs::rename(&temp, text)?;
    config::write_json(
        path,
        &json!({"state":state,"message":message,
        "helperProcessId":std::process::id(),"processId":server_pid}),
    )
}
async fn prepare(install: &Path, report: &Path, cancel: &Path) -> Result<()> {
    // A new installation has no owned executable/installed-instance mutex.
    if !install.join("TranscriptionServer.exe").exists() {
        return Ok(());
    }
    if !config::installed(install) {
        bail!("導入先のINSTALLATION.jsonを確認できません。上書きしません。")
    }
    if !platform::Instance::new()?.existing {
        return Ok(());
    }
    let cfg: config::Config = serde_json::from_value(config::read_json(
        &config::user_root().join("settings.json"),
    )?)?;
    if cfg.server {
        bail!("Bearer構成の停止は管理者がサーバーPCから実行してください。")
    }
    let scheme = if cfg.tls_cert.is_some() {
        "https"
    } else {
        "http"
    };
    let url = format!("{scheme}://127.0.0.1:{}", cfg.port);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let health: Value = client
        .get(format!("{url}/api/health"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pid = health["processId"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .context("サーバーPIDがありません")?;
    if health["appId"] != "meeting-transcriber"
        || health["runtime"] != "rust"
        || !same_image(
            &platform::image_path(pid)?,
            &install.join("TranscriptionServer.exe"),
        )?
    {
        bail!("接続先と導入先の所有サーバーが一致しません。停止しません。")
    }
    let identity =
        platform::identity(pid)?.context("サーバーが終了しました。再実行してください。")?;
    let token = health["token"]
        .as_str()
        .context("操作トークンがありません")?;
    let mut ticket = None;
    if health["capabilities"]["gracefulDrain"] == true {
        let accepted: Value = client
            .post(format!("{url}/api/admin/drain"))
            .header("X-Local-Token", token)
            .json(&json!({"action":"shutdown","confirm":"drain-server"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ticket = accepted["id"].as_str().map(String::from);
    }
    loop {
        if cancel.try_exists()? {
            if let Some(id) = &ticket {
                let _ = client
                    .post(format!("{url}/api/admin/drain/cancel"))
                    .header("X-Local-Token", token)
                    .json(&json!({"id":id}))
                    .send()
                    .await;
            }
            bail!("更新を取り消しました。停止の強制実行は行っていません。")
        }
        if platform::identity(pid)? != Some(identity) {
            if platform::Instance::new()?.existing {
                bail!("別の導入版サーバーが起動しています。更新を続行しません。")
            }
            return Ok(());
        }
        write_report(
            report,
            "waiting",
            "受付済み処理の完了とサーバーの正常終了を待っています。",
            Some(pid),
        )?;
        if ticket.is_none() {
            // Older servers cannot drain. Their shutdown endpoint atomically
            // refuses busy work; never kill them or claim that admission closed.
            let response = client
                .post(format!("{url}/api/admin/shutdown"))
                .header("X-Local-Token", token)
                .json(&json!({}))
                .send()
                .await?;
            if !response.status().is_success() && response.status() != reqwest::StatusCode::CONFLICT
            {
                bail!(
                    "旧版サーバーの正常停止要求が拒否されました: {}",
                    response.status()
                )
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn setup_control_reports_new_install_and_refuses_unowned_existing_program() {
        let root = std::env::temp_dir().join(format!("tc-setup-control-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let install = root.join("app");
        std::fs::create_dir(&install).unwrap();
        let report = root.join("result.json");
        let args = vec![
            "fixture".into(),
            "__prepare-setup".into(),
            install.to_string_lossy().into_owned(),
            report.to_string_lossy().into_owned(),
        ];
        run(&args).await.unwrap();
        assert_eq!(config::read_json(&report).unwrap()["state"], "complete");
        assert_eq!(
            config::read_json(&report).unwrap()["helperProcessId"],
            std::process::id()
        );
        assert_eq!(
            std::fs::read_to_string(report.with_extension("txt")).unwrap(),
            "対象サーバーの停止を確認しました。"
        );
        std::fs::write(install.join("TranscriptionServer.exe"), b"unowned fixture").unwrap();
        assert!(run(&args).await.is_err());
        assert_eq!(config::read_json(&report).unwrap()["state"], "error");
        assert!(
            std::fs::read_to_string(report.with_extension("txt"))
                .unwrap()
                .contains("INSTALLATION.json")
        );
        #[cfg(windows)]
        assert!(
            same_image(
                &platform::image_path(std::process::id()).unwrap(),
                &std::env::current_exe().unwrap()
            )
            .unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
