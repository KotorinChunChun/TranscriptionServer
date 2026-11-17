use crate::{
    app::App,
    config::{VERSION, read_json, write_json},
    platform,
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
const REPOSITORY: &str = "KotorinChunChun/TranscriptionServer";
const LATEST: &str =
    "https://api.github.com/repos/KotorinChunChun/TranscriptionServer/releases/latest";
pub const MARKER: &str = "<!-- transcription-server-runtime:rust -->";
const MAX_SIZE: u64 = 512 * 1024 * 1024;
pub fn version(value: &str) -> Result<[u16; 3]> {
    let pieces = value.split('.').collect::<Vec<_>>();
    if pieces.len() != 3
        || pieces
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("版番号はX.Y.Z形式です")
    }
    Ok([pieces[0].parse()?, pieces[1].parse()?, pieces[2].parse()?])
}
pub fn hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut block = [0; 256 * 1024];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        digest.update(&block[..n])
    }
    Ok(hex::encode(digest.finalize()))
}
pub fn status(app: &App) -> Result<Value> {
    let mut value = app.update_state.lock().unwrap().clone();
    value["installed"] = app.runtime.installed.into();
    if let Ok(handoff) = read_json(&app.runtime.home.join("updates/last-handoff.json"))
        && let Some(folder) = handoff["folder"].as_str()
    {
        let path = PathBuf::from(folder);
        if let (Ok(path), Ok(root)) = (
            std::fs::canonicalize(path),
            std::fs::canonicalize(app.runtime.home.join("updates")),
        ) && path.starts_with(root)
            && let Ok(outcome) = read_json(&path.join("result.json"))
            && matches!(
                outcome["state"].as_str(),
                Some("complete" | "rolled_back" | "failed")
            )
        {
            value["lastUpdate"] = json!({"state":outcome["state"],"message":outcome["message"],"version":outcome["version"]})
        }
    }
    Ok(value)
}
struct Busy<'a>(&'a AtomicBool);
impl<'a> Busy<'a> {
    fn new(flag: &'a AtomicBool) -> Result<Self> {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("更新の準備中です")
        };
        Ok(Self(flag))
    }
}
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release)
    }
}
pub fn parse(release: &Value, current: &str) -> Result<Option<Value>> {
    if release["draft"] == true || release["prerelease"] == true {
        bail!("安定版の更新情報ではありません")
    };
    // The user reset the product version for Rust. An old Python v4.x release is
    // never an update for this generation, regardless of numeric ordering.
    if !release["body"].as_str().unwrap_or("").contains(MARKER) {
        return Ok(None);
    }
    let tag = release["tag_name"]
        .as_str()
        .context("更新タグがありません")?;
    let target = tag.strip_prefix('v').context("更新タグが不正です")?;
    if version(target)? <= version(current)? {
        return Ok(None);
    }
    let name = format!("TranscriptionServer-v{target}-setup-x64.exe");
    let assets = release["assets"]
        .as_array()
        .context("更新assetがありません")?
        .iter()
        .filter(|a| a["name"] == name)
        .collect::<Vec<_>>();
    if assets.len() != 1 {
        bail!("更新インストーラーを確認できません")
    };
    let a = assets[0];
    let size = a["size"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= MAX_SIZE)
        .context("更新サイズが不正です")?;
    let digest = a["digest"]
        .as_str()
        .and_then(|s| s.strip_prefix("sha256:"))
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .context("SHA256情報が不正です")?;
    Ok(Some(
        json!({"version":target,"assetName":name,"size":size,"sha256":digest}),
    ))
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .user_agent("TranscriptionServer-rust-updater")
        .build()?)
}
async fn latest(current: &str) -> Result<Option<Value>> {
    let response = client()?
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("GitHubへ接続できません"))?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::FORBIDDEN
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0")
    {
        bail!("公開リリースの確認回数が上限に達しました。しばらく待ってから再確認してください。")
    }
    if !status.is_success() {
        bail!(
            "公開リリースを確認できませんでした（HTTP {}）",
            status.as_u16()
        )
    };
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(block) = stream.next().await {
        let block = block.map_err(|_| anyhow::anyhow!("更新情報を受信できません"))?;
        if bytes.len() + block.len() > 1024 * 1024 {
            bail!("更新情報が大きすぎます")
        };
        bytes.extend(block)
    }
    parse(
        &serde_json::from_slice::<Value>(&bytes).context("更新情報が不正です")?,
        current,
    )
}
async fn checked(app: &App) -> Result<Option<Value>> {
    app.update_state.lock().unwrap()["state"] = "checking".into();
    match latest(VERSION).await {
        Ok(manifest) => {
            let mut state = app.update_state.lock().unwrap();
            state["available"] = manifest.clone().unwrap_or(Value::Null);
            state["state"] = if manifest.is_some() {
                "available"
            } else {
                "current"
            }
            .into();
            state["lastChecked"] = crate::store::now().into();
            state["message"] = if manifest.is_some() {
                "新しいRust版があります。"
            } else {
                "このRust版の更新はありません。"
            }
            .into();
            Ok(manifest)
        }
        Err(error) => {
            let mut state = app.update_state.lock().unwrap();
            state["available"] = Value::Null;
            state["state"] = "error".into();
            state["message"] = error.to_string().into();
            Err(error)
        }
    }
}
pub async fn check(app: &App) -> Result<Value> {
    let _busy = Busy::new(&app.update_busy)?;
    checked(app).await?;
    status(app)
}
pub fn background(app: std::sync::Arc<App>) {
    if !app.runtime.installed {
        return;
    }
    tokio::spawn(async move {
        // 起動5秒後と、その後24時間ごとに更新を確認する。
        let mut wait = Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = app.stop.cancelled() => return,
                _ = tokio::time::sleep(wait) => (),
            }
            let _ = check(&app).await;
            wait = Duration::from_secs(86400);
        }
    });
}
pub async fn action(app: &App, path: &str, body: Value) -> Result<Value> {
    if !body.is_object() {
        bail!("JSON本文を指定してください")
    };
    match path {
        "/api/updates/check" => check(app).await,
        "/api/updates/apply" => {
            apply(
                app,
                body["version"]
                    .as_str()
                    .context("更新版を指定してください")?,
            )
            .await
        }
        _ => bail!("更新APIが見つかりません"),
    }
}
fn redirect(url: &str) -> Result<url::Url> {
    let url = url::Url::parse(url)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
    {
        bail!("更新ファイルの転送先は許可されていません")
    };
    Ok(url)
}
fn public_download(manifest: &Value) -> Result<(&str, url::Url)> {
    let target = manifest["version"].as_str().context("更新版が不正です")?;
    version(target)?;
    let name = manifest["assetName"]
        .as_str()
        .context("更新ファイル名が不正です")?;
    if name != format!("TranscriptionServer-v{target}-setup-x64.exe") {
        bail!("更新ファイル名が不正です")
    }
    Ok((
        name,
        url::Url::parse(&format!(
            "https://github.com/{REPOSITORY}/releases/download/v{target}/{name}"
        ))?,
    ))
}
async fn download(manifest: &Value, folder: &Path) -> Result<PathBuf> {
    let (name, mut url) = public_download(manifest)?;
    tokio::fs::create_dir_all(folder).await?;
    let path = folder.join(name);
    let size = manifest["size"].as_u64().context("更新サイズが不正です")?;
    let expected = manifest["sha256"].as_str().context("SHA256がありません")?;
    if path.is_file() && std::fs::metadata(&path)?.len() == size && hash(&path)? == expected {
        return Ok(path);
    }
    let temp = folder.join(format!("{name}.partial"));
    let result = async {
        let client = client()?;
        for _ in 0..6 {
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("更新ファイルを取得できません"))?;
            if response.status().is_redirection() {
                let value = response
                    .headers()
                    .get("Location")
                    .and_then(|v| v.to_str().ok())
                    .context("更新転送先がありません")?;
                url = redirect(value)?;
                continue;
            }
            if !response.status().is_success() {
                bail!(
                    "更新ファイルの取得に失敗しました（HTTP {}）",
                    response.status().as_u16()
                )
            }
            let mut file = tokio::fs::File::create(&temp).await?;
            let mut total = 0u64;
            let mut digest = Sha256::new();
            let mut stream = response.bytes_stream();
            while let Some(data) = stream.next().await {
                let data = data.map_err(|_| anyhow::anyhow!("更新取得中に接続が切れました"))?;
                total += data.len() as u64;
                if total > size || total > MAX_SIZE {
                    bail!("更新サイズが上限を超えました")
                };
                digest.update(&data);
                file.write_all(&data).await?;
            }
            file.flush().await?;
            drop(file);
            if total != size || hex::encode(digest.finalize()) != expected {
                bail!("更新ファイルのサイズ/SHA256が一致しません")
            };
            tokio::fs::rename(&temp, &path).await?;
            return Ok::<_, anyhow::Error>(path.clone());
        }
        bail!("更新ファイルの転送回数が上限を超えました")
    }
    .await;
    let _ = tokio::fs::remove_file(temp).await;
    result
}
struct Handoff<'a> {
    app: &'a App,
    folder: PathBuf,
    helper: Option<tokio::process::Child>,
    accepted: bool,
}
impl Drop for Handoff<'_> {
    fn drop(&mut self) {
        if !self.accepted {
            let _ = write_json(
                &self.folder.join("cancelled.json"),
                &json!({"cancelled":true}),
            );
            if let Some(child) = &mut self.helper {
                let _ = child.start_kill();
            }
            let mut state = self.app.update_state.lock().unwrap();
            state["state"] = "error".into();
            state["message"] = "更新の適用を開始できませんでした。".into();
        }
    }
}
async fn apply(app: &App, target: &str) -> Result<Value> {
    if !app.runtime.installed {
        bail!("自己更新はインストーラー版で利用できます")
    };
    {
        let _gate = app.admission.lock().await;
        if app.maintenance.load(Ordering::Acquire) || app.draining.load(Ordering::Acquire) {
            bail!("準備・停止・更新操作中です")
        }
    }
    let _busy = Busy::new(&app.update_busy)?;
    let manifest = checked(app)
        .await?
        .context("更新情報を再確認してください")?;
    if manifest["version"] != target {
        bail!("更新情報を再確認してください")
    }
    {
        let mut state = app.update_state.lock().unwrap();
        state["state"] = "downloading".into();
        state["message"] = "更新ファイルを取得しています。".into();
    }
    let folder = app
        .runtime
        .home
        .join("updates")
        .join(format!("{target}-{}", uuid::Uuid::new_v4().simple()));
    let mut handoff = Handoff {
        app,
        folder: folder.clone(),
        helper: None,
        accepted: false,
    };
    let installer = download(&manifest, &folder).await?;
    let mut drain = crate::lifecycle::Drain::begin(app, "update", true).await?;
    {
        let mut state = app.update_state.lock().unwrap();
        state["state"] = "draining".into();
        state["message"] = "新規受付を停止し、受付済みジョブの完了を待っています。".into();
    }
    drain.wait().await?;
    let helper = folder.join("TranscriptionUpdater.exe");
    tokio::fs::copy(std::env::current_exe()?, &helper).await?;
    let pid = std::process::id();
    let restart_args = std::env::args().skip(1).collect::<Vec<_>>();
    let health = format!(
        "{}://127.0.0.1:{}/api/health",
        if app.runtime.config.tls_cert.is_some() {
            "https"
        } else {
            "http"
        },
        app.runtime.config.port
    );
    let plan = json!({"installDir":app.runtime.root,"dataDir":app.store.dir,
        "installer":installer,"sha256":manifest["sha256"],"version":target,
        "currentVersion":VERSION,"parentPid":pid,"parentCreated":platform::identity(pid)?,
        "restartArgs":restart_args,"healthUrl":health,"handshake":true});
    write_json(&folder.join("plan.json"), &plan)?;
    crate::update_apply::validate(&plan, &folder)?;
    let mut command = platform::command(&helper);
    command
        .args([
            "__apply-update",
            &folder.join("plan.json").to_string_lossy(),
        ])
        .current_dir(&folder)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);
    #[cfg(windows)]
    command.creation_flags(0x08000208);
    handoff.helper = Some(command.spawn()?);
    let mut ready = false;
    for _ in 0..100 {
        if let Ok(result) = read_json(&folder.join("result.json"))
            && result["state"] == "waiting_for_exit"
        {
            ready = true;
            break;
        }
        if handoff.helper.as_mut().unwrap().try_wait()?.is_some() {
            bail!("更新ヘルパーを開始できません")
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        bail!("更新ヘルパーの準備を確認できません")
    };
    drain.seal().await?;
    write_json(
        &app.runtime.home.join("updates/last-handoff.json"),
        &json!({"folder":folder}),
    )?;
    write_json(
        &folder.join("approved.json"),
        &json!({"version":target,"sha256":manifest["sha256"]}),
    )?;
    handoff.accepted = true;
    drain.commit();
    {
        let mut state = app.update_state.lock().unwrap();
        state["state"] = "applying".into();
        state["message"] = "更新して再起動します。".into();
    }
    let stop = app.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        stop.cancel();
    });
    status(app)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"draft":false,"prerelease":false,"tag_name":"v0.2.0","body":MARKER,"assets":[{"id":1,"name":"TranscriptionServer-v0.2.0-setup-x64.exe","size":10,"digest":format!("sha256:{}","a".repeat(64))}]})
    }
    #[test]
    fn generations_and_semver_cannot_downgrade_or_select_python() {
        assert!(
            parse(&json!({"tag_name":"v4.2.0","body":"legacy"}), "0.1.0")
                .unwrap()
                .is_none()
        );
        assert!(parse(&fixture(), "0.1.0").unwrap().is_some());
        assert!(parse(&fixture(), "0.2.0").unwrap().is_none());
        for value in ["1.2.3-beta", "+1.2.3", "1.2", "70000.1.1"] {
            assert!(version(value).is_err())
        }
        let mut r = fixture();
        r["draft"] = true.into();
        assert!(parse(&r, "0.1.0").is_err());
    }
    #[test]
    fn redirects_are_https_and_fixed_hosts() {
        for value in [
            "http://api.github.com/a",
            "https://api.github.com/a",
            "https://github.com/another/repository/releases/download/v1/installer.exe",
            "https://evil.test/a",
            "https://user@api.github.com/a",
            "https://api.github.com:8443/a",
        ] {
            assert!(redirect(value).is_err())
        }
        assert!(
            redirect("https://release-assets.githubusercontent.com/a?temporary-signature=x")
                .is_ok()
        );
    }
    #[test]
    fn public_manifest_exposes_only_installer_identity() {
        let mut release = fixture();
        release["body"] = format!("{MARKER}\nhttps://example.invalid/release-notes").into();
        release["assets"][0]["browser_download_url"] = "https://example.invalid/setup.exe".into();
        let mut manifest = parse(&release, "0.1.18").unwrap().unwrap();
        assert_eq!(manifest.as_object().unwrap().len(), 4);
        let (name, url) = public_download(&manifest).unwrap();
        assert_eq!(url.host_str(), Some("github.com"));
        assert_eq!(
            url.path(),
            format!("/{REPOSITORY}/releases/download/v0.2.0/{name}")
        );
        assert!(url.query().is_none());
        manifest["assetName"] = "../unknown.exe".into();
        assert!(public_download(&manifest).is_err());
    }
    #[tokio::test]
    #[ignore = "公開サービスへ匿名でアクセスして実際の配布物を照合する"]
    async fn publicly_served_release_installer_downloads_anonymously() {
        let manifest = latest("0.0.0")
            .await
            .unwrap()
            .expect("公開配布物が必要です");
        let folder = std::env::temp_dir().join(format!(
            "transcription-update-test-{}",
            uuid::Uuid::new_v4()
        ));
        let result = download(&manifest, &folder).await;
        if let Ok(path) = &result {
            println!(
                "公開インストーラーの取得とSHA-256照合: v{}、{} bytes",
                manifest["version"].as_str().unwrap(),
                std::fs::metadata(path).unwrap().len()
            );
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(folder).ok();
        result.unwrap();
    }
}
