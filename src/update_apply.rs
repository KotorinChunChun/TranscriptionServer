use crate::{
    config::{installed, read_json, write_json},
    platform,
    updates::{hash, version},
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub install_dir: PathBuf,
    pub data_dir: PathBuf,
    pub installer: PathBuf,
    pub sha256: String,
    pub version: String,
    pub current_version: String,
    pub parent_pid: u32,
    pub parent_created: u64,
    pub restart_args: Vec<String>,
    pub health_url: String,
    #[serde(default)]
    pub handshake: bool,
    #[serde(default = "yes")]
    pub shortcuts: bool,
}
fn yes() -> bool {
    true
}
pub fn validate(value: &Value, folder: &Path) -> Result<Plan> {
    let mut plan: Plan = serde_json::from_value(value.clone()).context("更新計画が不正です")?;
    plan.install_dir = fs::canonicalize(&plan.install_dir)?;
    plan.data_dir = fs::canonicalize(&plan.data_dir)?;
    plan.installer = fs::canonicalize(&plan.installer)?;
    let folder = fs::canonicalize(folder)?;
    let root = &plan.install_dir;
    if root.components().count() < 3
        || !installed(root)
        || !(root.join("TranscriptionServer.exe").is_file())
    {
        bail!("導入先を確認できません")
    };
    if plan.data_dir.starts_with(root)
        || root.starts_with(&plan.data_dir)
        || folder.starts_with(root)
        || root.starts_with(&folder)
    {
        bail!("データ/更新キャッシュはプログラムの外へ配置してください")
    }
    if plan.installer.parent() != Some(folder.as_path())
        || plan.installer.file_name().and_then(|n| n.to_str())
            != Some(&format!(
                "TranscriptionServer-v{}-setup-x64.exe",
                plan.version
            ))
    {
        bail!("更新ファイルの位置が不正です")
    };
    if version(&plan.version)? <= version(&plan.current_version)? {
        bail!("旧版/同版への自己更新は行いません")
    };
    let build = read_json(&root.join("BUILD.json"))?;
    if build["version"] != plan.current_version || build["runtime"] != "rust" {
        bail!("導入版と更新計画の世代/版が一致しません")
    }
    let url = url::Url::parse(&plan.health_url)?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        || url.port().is_none()
        || url.path() != "/api/health"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("起動確認先はloopbackのhealthを指定してください")
    }
    if plan.parent_pid == 0
        || plan.parent_created == 0
        || plan.restart_args.iter().any(|s| s.contains('\0'))
        || plan.restart_args.iter().map(|s| s.len() + 3).sum::<usize>() > 30000
        || hash(&plan.installer)? != plan.sha256
    {
        bail!("PID/再起動設定/インストーラーのSHA256が不正です")
    };
    Ok(plan)
}
fn reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}
pub fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    if reparse(&fs::symlink_metadata(source)?) {
        bail!("プログラム内のリンクを検出しました")
    };
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if reparse(&meta) {
            bail!("プログラム内のリンクを検出しました")
        };
        let destination = target.join(entry.file_name());
        if meta.is_dir() {
            copy_tree(&entry.path(), &destination)?
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}
fn report(folder: &Path, state: &str, message: &str, plan: &Plan, extra: Value) -> Result<()> {
    let mut value = json!({"state":state,"message":message,"version":plan.version});
    for (k, v) in extra.as_object().context("更新結果が不正です")? {
        value[k] = v.clone()
    }
    write_json(&folder.join("result.json"), &value)
}
async fn wait_parent(plan: &Plan) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(90) {
        if platform::identity(plan.parent_pid)? != Some(plan.parent_created) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("サーバーの終了を確認できないため適用を中止しました")
}
struct Started {
    child: tokio::process::Child,
    scope: platform::Job,
}
fn start(plan: &Plan, folder: &Path, label: &str) -> Result<Started> {
    let out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join(format!("{label}.out.log")))?;
    let err = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join(format!("{label}.err.log")))?;
    let mut command = platform::command(&plan.install_dir.join("TranscriptionServer.exe"));
    command
        .args(&plan.restart_args)
        .current_dir(&plan.install_dir)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .kill_on_drop(false);
    let child = command.spawn()?;
    let scope = platform::Job::new()?;
    scope.assign(&child)?;
    Ok(Started { child, scope })
}
async fn stop_owned(instance: &mut Started) -> Result<()> {
    instance.scope.terminate()?;
    let _ = tokio::time::timeout(Duration::from_secs(10), instance.child.wait()).await;
    for _ in 0..100 {
        if instance.scope.empty()? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("更新後プロセスの終了を確認できません。自動復元を中止しました")
}
async fn healthy(plan: &Plan, instance: &mut Started, expected: &str) -> Result<bool> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    for _ in 0..200 {
        if instance.child.try_wait()?.is_some() {
            return Ok(false);
        }
        let mut request = client.get(&plan.health_url);
        if let Ok(key) = std::env::var("MEETING_SERVER_API_KEY") {
            request = request.bearer_auth(key)
        }
        if let Ok(response) = request.send().await
            && response.status().is_success()
            && let Ok(value) = response.json::<Value>().await
            && value["appId"] == "meeting-transcriber"
            && value["runtime"] == "rust"
            && value["serverVersion"] == expected
            && value["processId"].as_u64() == instance.child.id().map(u64::from)
        {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok(false)
}
pub async fn apply(path: &Path) -> Result<i32> {
    let path = fs::canonicalize(path)?;
    let folder = path
        .parent()
        .context("更新フォルダーが不正です")?
        .to_path_buf();
    let plan = validate(&read_json(&path)?, &folder)?;
    let backup = folder.join("program-backup");
    if backup.exists() {
        bail!("既存バックアップを保持するため適用を中止しました")
    };
    if fs2::available_space(&folder)?
        < crate::store::tree_size(&plan.install_dir)?
            + fs::metadata(&plan.installer)?.len()
            + crate::store::tree_size(&plan.data_dir)?
            + 256 * 1024 * 1024
    {
        bail!("バックアップの空き容量が不足しています")
    };
    report(
        &folder,
        "waiting_for_exit",
        "サーバーの終了を待っています。",
        &plan,
        json!({}),
    )?;
    let mut parent_gone = false;
    let mut backed_up = false;
    let mut launched: Option<Started> = None;
    let result = async {
        if plan.handshake {
            let mut accepted = false;
            for _ in 0..150 {
                if folder.join("cancelled.json").exists() {
                    bail!("更新要求が取り消されました")
                };
                if let Ok(approval) = read_json(&folder.join("approved.json"))
                    && approval["version"] == plan.version
                    && approval["sha256"] == plan.sha256
                {
                    accepted = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !accepted {
                bail!("更新の承認引き継ぎを確認できません")
            }
        }
        wait_parent(&plan).await?;
        parent_gone = true;
        tokio::time::sleep(Duration::from_millis(300)).await;
        copy_tree(&plan.install_dir, &backup)?;
        backed_up = true;
        let database = plan.data_dir.join("workbench.sqlite3");
        if database.is_file() {
            let db = rusqlite::Connection::open_with_flags(
                database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            db.backup(
                "main",
                folder.join("workbench-backup.sqlite3"),
                rusqlite::backup::NO_PROGRESS,
            )?;
        }
        report(
            &folder,
            "installing",
            "インストーラーを適用しています。",
            &plan,
            json!({}),
        )?;
        let mut cmd = platform::command(&plan.installer);
        cmd.args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/SP-",
            "/CURRENTUSER",
        ])
        .arg(format!(
            "/DIR={}",
            crate::config::friendly(&plan.install_dir).display()
        ))
        .arg(format!("/LOG={}", folder.join("install.log").display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
        for (key, value) in std::env::vars_os() {
            if [
                "SYSTEMROOT",
                "WINDIR",
                "PATH",
                "TEMP",
                "TMP",
                "LOCALAPPDATA",
                "APPDATA",
                "USERPROFILE",
                "USERNAME",
                "COMSPEC",
            ]
            .iter()
            .any(|k| key.to_string_lossy().eq_ignore_ascii_case(k))
            {
                cmd.env(key, value);
            }
        }
        if !plan.shortcuts {
            cmd.arg("/NOICONS");
        }
        let mut installer = cmd.spawn()?;
        let scope = platform::Job::new()?;
        scope.assign(&installer)?;
        let exit = match tokio::time::timeout(Duration::from_secs(600), installer.wait()).await {
            Ok(value) => value?,
            Err(_) => {
                scope.terminate()?;
                let _ = installer.wait().await;
                for _ in 0..100 {
                    if scope.empty()? {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if !scope.empty()? {
                    bail!("インストーラーの終了を確認できません")
                };
                bail!("インストールがタイムアウトしました")
            }
        };
        if !exit.success() {
            bail!("インストーラーが正常終了しませんでした")
        };
        drop(scope);
        launched = Some(start(&plan, &folder, "updated")?);
        let process = launched.as_mut().unwrap();
        if !healthy(&plan, process, &plan.version).await? {
            bail!("更新後の起動を確認できません")
        }
        let pid = process.child.id().context("再起動したPIDがありません")?;
        process.scope.detach()?;
        report(
            &folder,
            "complete",
            "更新して再起動しました。",
            &plan,
            json!({"backup":backup,"serverPid":pid,"serverCreated":platform::identity(pid)?}),
        )?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        fs::write(folder.join("error.log"), format!("{error:#}\n"))?;
        if let Some(process) = &mut launched {
            stop_owned(process).await?
        }
        if !parent_gone {
            report(
                &folder,
                "failed",
                "サーバーの終了を確認できないため適用を中止しました。",
                &plan,
                json!({}),
            )?;
            return Ok(1);
        }
        if backed_up {
            if !installed(&backup)
                || fs::canonicalize(&plan.install_dir)? != plan.install_dir
                || reparse(&fs::symlink_metadata(&plan.install_dir)?)
            {
                bail!("復元先を確認できません")
            };
            fs::remove_dir_all(&plan.install_dir)?;
            copy_tree(&backup, &plan.install_dir)?;
            let _ = platform::restore_display_version(&plan.install_dir, &plan.current_version);
        }
        let mut restored = start(&plan, &folder, "restored")?;
        let healthy = healthy(&plan, &mut restored, &plan.current_version).await?;
        let pid = restored.child.id();
        if healthy {
            restored.scope.detach()?
        } else {
            stop_owned(&mut restored).await?
        }
        report(
            &folder,
            if healthy { "rolled_back" } else { "failed" },
            if healthy {
                "更新に失敗しました。旧版のプログラムを復元して起動しました。"
            } else {
                "更新と旧版の再起動を確認できません。更新記録を確認してください。"
            },
            &plan,
            json!({"serverPid":pid,"serverCreated":pid.map(platform::identity).transpose()?.flatten()}),
        )?;
        return Ok(1);
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_rejects_hash_paths_and_downgrades() {
        let base = std::env::temp_dir().join(format!("tc-update-{}", uuid::Uuid::new_v4()));
        let root = base.join("app");
        let data = base.join("data");
        let folder = base.join("update");
        for p in [&root, &data, &folder] {
            fs::create_dir_all(p).unwrap()
        }
        write_json(
            &root.join("INSTALLATION.json"),
            &json!({"product":"TranscriptionServer","scope":"user","appId":crate::config::APP_ID}),
        )
        .unwrap();
        write_json(
            &root.join("BUILD.json"),
            &json!({"version":"0.1.0","runtime":"rust"}),
        )
        .unwrap();
        fs::write(root.join("TranscriptionServer.exe"), b"old").unwrap();
        let installer = folder.join("TranscriptionServer-v0.2.0-setup-x64.exe");
        fs::write(&installer, b"fixture").unwrap();
        let plan = json!({"installDir":root,"dataDir":data,"installer":installer,"sha256":hash(&installer).unwrap(),"version":"0.2.0","currentVersion":"0.1.0","parentPid":1,"parentCreated":1,"restartArgs":[],"healthUrl":"http://127.0.0.1:8769/api/health"});
        assert!(validate(&plan, &folder).is_ok());
        let mut tls_plan = plan.clone();
        tls_plan["healthUrl"] = json!("https://127.0.0.1:8769/api/health");
        assert!(validate(&tls_plan, &folder).is_ok());
        for (key, value) in [
            ("sha256", json!("bad")),
            ("dataDir", json!(root.join("data"))),
            ("version", json!("0.0.1")),
            ("healthUrl", json!("http://external.test:8769/api/health")),
        ] {
            let mut altered = plan.clone();
            altered[key] = value;
            assert!(validate(&altered, &folder).is_err());
        }
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn deep_backup_preserves_bytes() {
        let base = std::env::temp_dir().join(format!("tc-copy-{}", uuid::Uuid::new_v4()));
        let source = base.join("source");
        let relative = PathBuf::from("nested")
            .join("long-folder-name-".repeat(8))
            .join("another-folder-name-".repeat(5));
        fs::create_dir_all(source.join(&relative)).unwrap();
        fs::write(source.join(&relative).join("file.bin"), b"keep").unwrap();
        copy_tree(&source, &base.join("backup")).unwrap();
        assert_eq!(
            fs::read(base.join("backup").join(relative).join("file.bin")).unwrap(),
            b"keep"
        );
        fs::remove_dir_all(base).unwrap();
    }
}
