use crate::{app::App, config, platform};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub struct Package {
    pub id: &'static str,
    pub label: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub max_download: u64,
    pub programs: &'static [(&'static str, &'static str)],
}
const MIB: u64 = 1024 * 1024;
const MAX_UNPACK: u64 = 1024 * MIB;
pub const PACKAGES: [Package; 4] = [
    Package {
        id: "whisper",
        label: "Whisper専用実行環境",
        version: "faster-whisper-1.2.1-py314",
        url: "https://www.python.org/downloads/release/python-31410/",
        sha256: "bdfeedeea0cb44ba4a008ca685583fdd7922282872975911c12e4830c3aae536",
        max_download: 130 * MIB,
        programs: &[("python", "--version")],
    },
    Package {
        id: "speech",
        label: "speech.cpp",
        version: "0.10.1",
        url: "https://github.com/nyosegawa/speech.cpp/releases/download/v0.10.1/speech-0.10.1-windows-x64-vulkan.zip",
        sha256: "a8fe0e4b4f463c460e7ace9d38c363bec08b254cac5a7f764b7967af35d4fa1a",
        max_download: 50 * MIB,
        programs: &[("speech", "--version")],
    },
    Package {
        id: "nemo",
        label: "NeMo-Speech.cpp",
        version: "0.3.0",
        url: "https://github.com/NVIDIA/NeMo-Speech.cpp/releases/download/v0.3.0/nemo-speech-0.3.0-windows-x86_64-vulkan.zip",
        sha256: "72b3b8b41bfe2be0320eff4e9d310594e4f99deefcde4a157799a039656963ce",
        max_download: 30 * MIB,
        programs: &[("nemo-speech", "--version")],
    },
    Package {
        id: "ffmpeg",
        label: "FFmpeg / FFprobe",
        version: "8.1.2",
        url: "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-8.1.2-essentials_build.zip",
        sha256: "db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec",
        max_download: 160 * MIB,
        programs: &[("ffmpeg", "-version"), ("ffprobe", "-version")],
    },
];
pub fn package(id: &str) -> Result<&'static Package> {
    PACKAGES
        .iter()
        .find(|p| p.id == id)
        .context("管理対象外の実行環境です")
}
pub(crate) fn target(home: &Path, p: &Package) -> PathBuf {
    home.join("runtimes")
        .join(format!("{}-{}-{}", p.id, p.version, &p.sha256[..8]))
}
fn safe_name(name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name.is_ascii()
        || name.contains(['\\', ':', '\0', '*', '?', '"', '<', '>', '|'])
        || name.starts_with('/')
    {
        bail!("ZIP内のパスが不正です")
    }
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("ZIP内のパスが空です")
    }
    for part in trimmed.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with(['.', ' '])
            || part.chars().any(|c| c < ' ')
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            bail!("ZIP内のファイル名が不正です")
        }
    }
    let path = PathBuf::from(trimmed);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("ZIPの展開先が不正です")
    }
    Ok(path)
}
pub fn managed_program(home: &Path, id: &str, name: &str) -> Option<PathBuf> {
    let p = package(id).ok()?;
    if !p.programs.iter().any(|r| r.0 == name) {
        return None;
    }
    let root = target(home, p);
    let receipt = config::read_json(&root.join("receipt.json")).ok()?;
    if id == "whisper" {
        let worker = fs::read(root.join("whisper_worker.py")).ok()?;
        if format!("{:x}", Sha256::digest(&worker)) != receipt["workerSha256"].as_str()? {
            return None;
        }
    }
    if receipt["id"] != id
        || receipt["version"] != p.version
        || receipt["archiveSha256"] != p.sha256
    {
        return None;
    }
    let relative = safe_name(receipt["programs"][name].as_str()?).ok()?;
    let path = root.join(relative);
    let canonical = path.canonicalize().ok()?;
    (canonical.starts_with(root.canonicalize().ok()?) && canonical.is_file()).then_some(path)
}
fn detected(app: &App, id: &str) -> bool {
    match id {
        "whisper" => app.tools().whisper.is_some(),
        "speech" => app.tools().speech.is_some(),
        "nemo" => app.tools().nemo.is_some(),
        "ffmpeg" => app.tools().ffmpeg.is_some() && app.tools().ffprobe.is_some(),
        _ => false,
    }
}
fn managed_only(app: &App, id: &str) -> bool {
    let tools = app.tools();
    let root = target(&app.runtime.home, package(id).unwrap());
    let paths: Vec<&Path> = match id {
        "whisper" => tools
            .whisper
            .as_ref()
            .map(|v| v.0.as_path())
            .into_iter()
            .collect(),
        "speech" => tools.speech.as_deref().into_iter().collect(),
        "nemo" => tools.nemo.as_deref().into_iter().collect(),
        "ffmpeg" => [tools.ffmpeg.as_deref(), tools.ffprobe.as_deref()]
            .into_iter()
            .flatten()
            .collect(),
        _ => return false,
    };
    paths.into_iter().all(|p| {
        p.starts_with(&root)
            || p.canonicalize()
                .ok()
                .zip(root.canonicalize().ok())
                .is_some_and(|(p, r)| p.starts_with(r))
    })
}
pub async fn refresh_health(app: &App, engines: &Value, native: &Value) {
    let engine = |id: &str| {
        engines
            .as_array()
            .and_then(|r| r.iter().find(|e| e["id"] == id))
    };
    let media = |path: Option<PathBuf>| async move {
        if let Some(path) = path {
            probe(&path, "-version", "version").await.is_ok()
        } else {
            false
        }
    };
    let (ffmpeg, ffprobe) = tokio::join!(
        media(app.tools().ffmpeg.clone()),
        media(app.tools().ffprobe.clone())
    );
    let has_rows = |value: &Value| value.as_array().is_some_and(|r| !r.is_empty());
    let speech = has_rows(&native["models"]);
    let whisper = engine("whisper").is_some_and(|e| has_rows(&e["models"]));
    let nemo = engine(crate::nemo::ENGINE).is_some_and(|e| e["runtimeHealthy"] == true);
    *app.runtime_health.lock().unwrap() =
        json!({"speech":speech,"whisper":whisper,"nemo":nemo,"ffmpeg":ffmpeg&&ffprobe});
}
pub fn inventory(app: &App) -> Value {
    let health = app.runtime_health.lock().unwrap().clone();
    let pending = app.runtime_pending_restart.lock().unwrap().clone();
    let rows = PACKAGES.iter().map(|p| {
        let present = detected(app, p.id);
        let ready = present && health[p.id] == true;
        let staged = p
            .programs
            .iter()
            .all(|r| managed_program(&app.runtime.home, p.id, r.0).is_some());
        let restart = pending.contains(p.id) || (!present && staged);
        let broken = (present && !ready) || (!staged && target(&app.runtime.home, p).exists());
        let owned = managed_only(app, p.id);
        let state = if restart {
            "restart_required"
        } else if ready {
            "detected"
        } else if broken {
            "runtime_error"
        } else {
            "missing"
        };
        let message = if broken && !owned {
            "設定された外部実行環境の点検に失敗しました。外部配置は自動修復しません。パスと依存ライブラリを確認してください。"
        } else if broken {
            "実行環境を修復できます。旧配置は退避し、モデルと録音は保持します。"
        } else {
            ""
        };
        json!({"id":p.id,"label":p.label,"version":p.version,"state":state,
            "detected":present,"healthy":ready&&!restart,"repairable":broken&&owned&&!restart,
            "installable":cfg!(windows)&&!ready&&!restart&&owned,"restartRequired":restart,
            "statusMessage":message,"maxDownloadBytes":p.max_download,"sourceUrl":p.url})
    });
    json!(rows.collect::<Vec<_>>())
}
struct OwnedStage {
    base: PathBuf,
    path: PathBuf,
}
impl Drop for OwnedStage {
    fn drop(&mut self) {
        // Only the freshly-created direct child owned by this operation may be removed.
        if self.path.parent() == Some(self.base.as_path())
            && self
                .path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".install-"))
            && self
                .path
                .canonicalize()
                .is_ok_and(|p| p.starts_with(&self.base))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
fn progress(app: &App, phase: &str, received: u64, total: u64) {
    let mut value = app.management.lock().unwrap();
    value["phase"] = phase.into();
    value["receivedBytes"] = received.into();
    value["totalBytes"] = total.into();
    value["message"] = match phase {
        "download" => "実行環境を取得しています。",
        "extract" => "実行環境を検証・展開しています。",
        _ => "実行環境を点検しています。",
    }
    .into();
}
fn check_digest(actual: &[u8], expected: &str) -> Result<()> {
    if hex::encode(actual) != expected {
        bail!("実行環境のSHA256が一致しません。取得物は実行していません。")
    }
    Ok(())
}
fn extract(zip_path: &Path, dest: &Path, cancel: &CancellationToken) -> Result<()> {
    let mut zip = zip::ZipArchive::new(fs::File::open(zip_path)?)?;
    if zip.len() > 10000 {
        bail!("ZIPのファイル数が上限を超えています")
    }
    let mut total = 0u64;
    let mut seen = HashSet::new();
    for i in 0..zip.len() {
        if cancel.is_cancelled() {
            bail!("実行環境の導入を中止しました")
        }
        let mut item = zip.by_index(i)?;
        let raw = std::str::from_utf8(item.name_raw())
            .context("ZIPのファイル名がUTF-8ではありません")?
            .to_owned();
        let relative = safe_name(&raw)?;
        if !seen.insert(relative.to_string_lossy().to_lowercase()) {
            bail!("ZIP内に重複パスがあります")
        }
        if let Some(mode) = item.unix_mode() {
            let kind = mode & 0o170000;
            if !matches!(kind, 0 | 0o100000 | 0o040000) {
                bail!("ZIPのリンク・特殊ファイルは展開しません")
            }
        }
        total = total
            .checked_add(item.size())
            .context("ZIPの展開容量が不正です")?;
        if total > MAX_UNPACK {
            bail!("ZIPの展開容量が上限を超えています")
        }
        let path = dest.join(relative);
        if item.is_dir() {
            fs::create_dir_all(path)?;
            continue;
        }
        fs::create_dir_all(path.parent().context("ZIPの親パスが不正です")?)?;
        let expected = item.size();
        let mut written = 0u64;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut buffer = [0u8; 65536];
        loop {
            if cancel.is_cancelled() {
                bail!("実行環境の導入を中止しました")
            }
            let n = item.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > expected {
                bail!("ZIPのファイル容量が指定値を超えています")
            }
            file.write_all(&buffer[..n])?;
        }
        if written != expected {
            bail!("ZIPのファイルが不完全です")
        }
        file.sync_all()?;
    }
    Ok(())
}
fn locate(root: &Path, name: &str) -> Result<PathBuf> {
    let filename = format!("{name}.exe");
    let mut dirs = vec![root.to_path_buf()];
    let mut found = vec![];
    while let Some(dir) = dirs.pop() {
        for item in fs::read_dir(dir)? {
            let item = item?;
            let kind = item.file_type()?;
            if kind.is_dir() {
                dirs.push(item.path())
            } else if kind.is_file()
                && item
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&filename)
            {
                found.push(item.path())
            }
        }
    }
    if found.len() != 1 {
        bail!("実行環境に必要な{filename}が一意に見つかりません")
    }
    Ok(found.remove(0))
}
async fn probe(path: &Path, flag: &str, version: &str) -> Result<()> {
    let scope = platform::Job::new()?;
    let mut cmd = platform::command(path);
    cmd.arg(flag)
        .env("SPEECH_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .context("実行環境を起動できません。必要なOSライブラリを確認してください")?;
    scope.assign(&child)?;
    let stdout = child.stdout.take().context("点検出力がありません")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = vec![];
        stdout.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536
            || !child.wait().await?.success()
            || !String::from_utf8_lossy(&bytes).contains(version)
        {
            bail!("実行環境の起動・版点検に失敗しました")
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("実行環境の点検がタイムアウトしました")??;
    Ok(())
}
async fn promote(source: &Path, destination: &Path, cancel: &CancellationToken) -> Result<()> {
    for attempt in 0..100 {
        if cancel.is_cancelled() {
            bail!("実行環境の導入を中止しました")
        }
        if destination.exists() {
            bail!("実行環境の配置先が既に存在します。上書きしません。")
        }
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(e)
                if attempt < 99
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(32 | 33))) =>
            {
                // Windows can briefly retain an image/AV handle after the version probe exits.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => bail!("検証済み実行環境の配置に失敗しました: {e}"),
        }
    }
    unreachable!()
}
fn validate_target(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse {
        bail!("実行環境の配置先が通常の管理フォルダーではありません。変更しません。")
    }
    Ok(())
}
/// Validate the replacement first; retain the entire old runtime outside the active name.
async fn activate(source: &Path, destination: &Path, cancel: &CancellationToken) -> Result<()> {
    let base = destination
        .parent()
        .context("実行環境の配置先が不正です")?
        .canonicalize()?;
    if source
        .parent()
        .and_then(Path::parent)
        .context("展開先が不正です")?
        .canonicalize()?
        != base
    {
        bail!("実行環境の展開先が専用領域の外です")
    }
    let backup = if destination.exists() {
        validate_target(destination)?;
        let backup = base.join(format!(
            ".previous-{}-{}",
            destination.file_name().unwrap().to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        ));
        if cancel.is_cancelled() {
            bail!("実行環境の導入を中止しました")
        }
        fs::rename(destination, &backup).context(
            "旧実行環境を退避できません。ロード済みモデルを解放してから修復してください。旧配置は保持しました",
        )?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = promote(source, destination, cancel).await {
        if let Some(backup) = &backup {
            fs::rename(backup, destination).with_context(|| {
                format!(
                    "修復失敗後の旧配置復元に失敗しました。退避先 {}。元の失敗: {error}",
                    backup.display()
                )
            })?;
        }
        return Err(error);
    }
    Ok(())
}
pub async fn install(app: &App, id: &str, parent_cancel: &CancellationToken) -> Result<()> {
    if !cfg!(windows) {
        bail!("この実行環境の自動導入はWindows x64用です")
    }
    let p = package(id)?;
    if app.runtime_pending_restart.lock().unwrap().contains(id) {
        bail!("実行環境の反映には先にサーバーを再起動してください。")
    }
    if !managed_only(app, id) {
        bail!(
            "設定された外部実行環境は自動修復しません。配置パスと依存ライブラリを確認してください。"
        )
    }
    if detected(app, id) && app.runtime_health.lock().unwrap()[id] == true {
        bail!("実行環境は検出済みです。上書きしません。")
    }
    let cancel = parent_cancel.child_token();
    let _cancel = CancelOnDrop(cancel.clone());
    fs::create_dir_all(app.runtime.home.join("runtimes"))?;
    let base = fs::canonicalize(app.runtime.home.join("runtimes"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(base.join(".install.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("別プロセスが実行環境を導入中です")?;
    let final_dir = target(&app.runtime.home, p);
    if final_dir.exists() {
        if !detected(app, id)
            && p.programs
                .iter()
                .all(|r| managed_program(&app.runtime.home, id, r.0).is_some())
        {
            return Ok(());
        }
        validate_target(&final_dir)?;
    }
    if fs2::available_space(&base)? < MAX_UNPACK + p.max_download + 128 * MIB {
        bail!("実行環境を展開する空き容量が不足しています（約1.3GB必要）。")
    }
    let stage = Arc::new(OwnedStage {
        path: base.join(format!(".install-{}", uuid::Uuid::new_v4().simple())),
        base,
    });
    fs::create_dir(&stage.path)?;
    let archive = stage.path.join("archive.zip");
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(900))
        .redirect(reqwest::redirect::Policy::limited(6))
        .build()?;
    if id == "whisper" {
        let unpack = stage.path.join("unpacked");
        fs::create_dir(&unpack)?;
        install_whisper(app, &stage, &unpack, &cancel).await?;
        activate(&unpack, &final_dir, &cancel).await?;
        app.runtime_pending_restart
            .lock()
            .unwrap()
            .insert(id.into());
        return Ok(());
    }
    let response = client
        .get(p.url)
        .send()
        .await
        .context("実行環境の取得要求に失敗しました")?
        .error_for_status()
        .context("配布元から実行環境を取得できませんでした")?;
    let total = response.content_length().unwrap_or(p.max_download);
    if total > p.max_download {
        bail!("取得する実行環境の容量が上限を超えています")
    }
    let mut response = response.bytes_stream();
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut sha = Sha256::new();
    let mut received = 0u64;
    use tokio::io::AsyncWriteExt;
    progress(app, "download", 0, total);
    while let Some(bytes) = response.next().await {
        if cancel.is_cancelled() {
            bail!("実行環境の導入を中止しました")
        }
        let bytes = bytes?;
        received += bytes.len() as u64;
        if received > p.max_download {
            bail!("取得する実行環境の容量が上限を超えています")
        }
        sha.update(&bytes);
        file.write_all(&bytes).await?;
        progress(app, "download", received, total);
    }
    file.sync_all().await?;
    drop(file);
    check_digest(&sha.finalize(), p.sha256)?;
    progress(app, "extract", received, received);
    let unpack = stage.path.join("unpacked");
    fs::create_dir(&unpack)?;
    let owned = stage.clone();
    let token = cancel.clone();
    let extract_path = unpack.clone();
    tokio::task::spawn_blocking(move || {
        let result = extract(&archive, &extract_path, &token);
        drop(owned);
        result
    })
    .await
    .context("ZIP展開処理が終了できませんでした")?
    .map_err(|e| anyhow::anyhow!("ZIPの検証・展開に失敗しました: {e}"))?;
    let mut paths = serde_json::Map::new();
    progress(app, "probe", received, received);
    for (name, flag) in p.programs {
        if cancel.is_cancelled() {
            bail!("実行環境の導入を中止しました")
        }
        let path = locate(&unpack, name)?;
        probe(&path, flag, p.version).await?;
        paths.insert(
            (*name).into(),
            path.strip_prefix(&unpack)?
                .to_string_lossy()
                .replace('\\', "/")
                .into(),
        );
    }
    config::write_json(
        &unpack.join("receipt.json"),
        &json!({"id":id,"version":p.version,"archiveSha256":p.sha256,"sourceUrl":p.url,"programs":paths}),
    ).map_err(|e|anyhow::anyhow!("実行環境の導入記録を保存できませんでした: {e}"))?;
    // Atomic promotion only after every file, digest and program has passed validation.
    progress(app, "activate", received, received);
    activate(&unpack, &final_dir, &cancel).await?;
    app.runtime_pending_restart
        .lock()
        .unwrap()
        .insert(id.into());
    Ok(())
}

#[derive(serde::Deserialize)]
struct WhisperFile {
    filename: String,
    url: String,
    sha256: String,
    size: u64,
    kind: String,
}
#[derive(serde::Deserialize)]
struct WhisperManifest {
    version: String,
    files: Vec<WhisperFile>,
}
const WHISPER_MANIFEST: &str = include_str!("../manifests/whisper-windows-x64.json");
fn whisper_manifest() -> Result<WhisperManifest> {
    check_digest(
        &Sha256::digest(WHISPER_MANIFEST.replace("\r\n", "\n").as_bytes()),
        package("whisper")?.sha256,
    )?;
    let manifest: WhisperManifest = serde_json::from_str(WHISPER_MANIFEST)?;
    if manifest.version != package("whisper")?.version || manifest.files.len() != 26 {
        bail!("Whisper導入マニフェストが不正です")
    }
    for f in &manifest.files {
        let url = reqwest::Url::parse(&f.url)?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some("www.python.org" | "files.pythonhosted.org" | "github.com")
            )
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || !matches!(f.kind.as_str(), "python" | "wheel" | "crt")
            || safe_name(&f.filename)?.components().count() != 1
            || f.sha256.len() != 64
            || f.size == 0
        {
            bail!("Whisperの固定取得先が不正です")
        }
    }
    Ok(manifest)
}
async fn whisper_download(
    app: &App,
    client: &reqwest::Client,
    f: &WhisperFile,
    path: &Path,
    offset: u64,
    total: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    let response = tokio::select! {
        _ = cancel.cancelled() => bail!("Whisper実行環境の導入を中止しました"),
        r = client.get(&f.url).send() => r?,
    }
    .error_for_status()?;
    if response.content_length().is_some_and(|n| n != f.size) {
        bail!("Whisper配布元の取得サイズが固定値と異なります")
    }
    let mut bytes = response.bytes_stream();
    let mut file = tokio::fs::File::create(path).await?;
    let mut sha = Sha256::new();
    let mut received = 0;
    use tokio::io::AsyncWriteExt;
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => bail!("Whisper実行環境の導入を中止しました"),
            v = bytes.next() => v,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        received += chunk.len() as u64;
        if received > f.size {
            bail!("Whisper取得サイズの上限を超えました")
        }
        sha.update(&chunk);
        file.write_all(&chunk).await?;
        progress(app, "download", offset + received, total);
    }
    file.sync_all().await?;
    if received != f.size {
        bail!("Whisper取得ファイルが不完全です")
    }
    check_digest(&sha.finalize(), &f.sha256)
}
async fn install_whisper(
    app: &App,
    stage: &Arc<OwnedStage>,
    unpack: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let manifest = whisper_manifest()?;
    let total: u64 = manifest.files.iter().map(|f| f.size).sum();
    if total > package("whisper")?.max_download {
        bail!("Whisper取得容量が上限を超えています")
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(900))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() < 6
                && a.url().scheme() == "https"
                && matches!(
                    a.url().host_str(),
                    Some(
                        "www.python.org"
                            | "files.pythonhosted.org"
                            | "github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                a.follow()
            } else {
                a.error("許可されないリダイレクトです")
            }
        }))
        .build()?;
    let mut offset = 0;
    let mut expanded = 0;
    for f in &manifest.files {
        let archive = stage.path.join(&f.filename);
        whisper_download(app, &client, f, &archive, offset, total, cancel).await?;
        offset += f.size;
        let destination = if f.kind == "python" {
            unpack.to_path_buf()
        } else if f.kind == "crt" {
            stage.path.join("crt")
        } else {
            unpack.join("Lib/site-packages")
        };
        fs::create_dir_all(&destination)?;
        progress(app, "extract", offset, total);
        let owned = stage.clone();
        let token = cancel.clone();
        let dest = destination.clone();
        let size = tokio::task::spawn_blocking(move || {
            let z = zip::ZipArchive::new(fs::File::open(&archive)?)?;
            let n = z.decompressed_size().context("ZIP展開容量が不明です")?;
            extract(&archive, &dest, &token)?;
            drop(owned);
            Ok::<_, anyhow::Error>(n)
        })
        .await??;
        expanded += size;
        if expanded > u128::from(MAX_UNPACK) {
            bail!("Whisper展開容量が上限を超えました")
        }
        if f.kind == "crt" {
            for name in [
                "msvcp140",
                "msvcp140_1",
                "msvcp140_2",
                "msvcp140_atomic_wait",
                "msvcp140_codecvt_ids",
            ] {
                let dll = locate_dll(&destination, &format!("{name}.dll"))?;
                fs::copy(dll, unpack.join(format!("{name}.dll")))?;
            }
            copy_notices(
                &destination,
                &destination,
                &unpack.join("licenses/nvidia-native-crt"),
            )?;
        }
    }
    fs::write(
        unpack.join("python314._pth"),
        "python314.zip\n.\nLib/site-packages\nimport site\n",
    )?;
    let worker = include_bytes!("../workers/whisper_worker.py");
    fs::write(unpack.join("whisper_worker.py"), worker)?;
    progress(app, "probe", total, total);
    let python = unpack.join("python.exe");
    probe(&python, "--version", "3.14.10").await?;
    let scope = platform::Job::new()?;
    let mut command = platform::command(&python);
    command
        .arg(unpack.join("whisper_worker.py"))
        .arg("--runtime-check")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1");
    let child = command.spawn()?;
    scope.assign(&child)?;
    let output = tokio::select! {
        _ = cancel.cancelled() => bail!("Whisper点検を中止しました"),
        r = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()) => {
            r.context("Whisper点検がタイムアウトしました")??
        }
    };
    if !output.status.success() {
        bail!("Whisperの依存ライブラリーを起動できません。専用領域の導入を再試行してください。")
    }
    let checked: Value = serde_json::from_slice(&output.stdout)?;
    if checked["ok"] != true {
        bail!("Whisperの固定依存点検に失敗しました")
    }
    config::write_json(
        &unpack.join("receipt.json"),
        &json!({"id":"whisper","version":package("whisper")?.version,
            "archiveSha256":package("whisper")?.sha256,"sourceUrl":package("whisper")?.url,
            "programs":{"python":"python.exe"},
            "workerSha256":format!("{:x}",Sha256::digest(worker)),"dependencies":checked,
            "downloadBytes":total,"expandedBytes":expanded}),
    )?;
    progress(app, "activate", total, total);
    Ok(())
}
fn locate_dll(root: &Path, name: &str) -> Result<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = vec![];
    while let Some(dir) = pending.pop() {
        for f in fs::read_dir(dir)? {
            let f = f?;
            if f.file_type()?.is_dir() {
                pending.push(f.path())
            } else if f.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
                found.push(f.path())
            }
        }
    }
    if found.len() != 1 {
        bail!("固定実行環境の必要DLLが一意ではありません: {name}")
    }
    Ok(found.remove(0))
}
fn copy_notices(root: &Path, dir: &Path, to: &Path) -> Result<()> {
    for f in fs::read_dir(dir)? {
        let f = f?;
        if f.file_type()?.is_dir() {
            copy_notices(root, &f.path(), to)?
        } else {
            let name = f.file_name().to_string_lossy().to_ascii_lowercase();
            if name.contains("license") || name.contains("notice") {
                let p = to.join(f.path().strip_prefix(root)?);
                fs::create_dir_all(p.parent().context("ライセンス保存先が不正です")?)?;
                fs::copy(f.path(), p)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn repair_keeps_backup_rolls_back_failure_and_refuses_nonowned_paths() {
        let root = std::env::temp_dir().join(format!("tc-repair-{}", uuid::Uuid::new_v4()));
        let stage = root.join(".install-test");
        let source = stage.join("unpacked");
        let destination = root.join("speech-test");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("program"), b"new").unwrap();
        fs::write(destination.join("program"), b"old").unwrap();
        activate(&source, &destination, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fs::read(destination.join("program")).unwrap(), b"new");
        let backups = || {
            fs::read_dir(&root)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".previous-"))
                .map(|e| e.path())
                .collect::<Vec<_>>()
        };
        assert_eq!(backups().len(), 1);
        assert_eq!(fs::read(backups()[0].join("program")).unwrap(), b"old");
        assert!(
            activate(&source, &destination, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(fs::read(destination.join("program")).unwrap(), b"new");
        assert_eq!(backups().len(), 1);
        fs::create_dir(&source).unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(activate(&source, &destination, &cancelled).await.is_err());
        assert_eq!(fs::read(destination.join("program")).unwrap(), b"new");
        assert!(
            activate(&root, &destination, &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(validate_target(&destination.join("program")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn promotion_waits_for_owned_image_handle_without_replacing_existing_directory() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!(
            "transcription-promote-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir_all(&source).unwrap();
        let file = source.join("probe.exe");
        fs::write(&file, b"fixture").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            drop(held)
        });
        promote(&source, &target, &CancellationToken::new())
            .await
            .unwrap();
        release.join().unwrap();
        assert_eq!(fs::read(target.join("probe.exe")).unwrap(), b"fixture");
        fs::create_dir(&source).unwrap();
        assert!(
            promote(&source, &target, &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn whisper_recipe_has_fixed_sources_and_complete_download_budget() {
        let manifest = whisper_manifest().unwrap();
        assert_eq!(
            manifest.files.iter().map(|f| f.size).sum::<u64>(),
            125_450_083
        );
        assert_eq!(
            manifest.files.iter().filter(|f| f.kind == "python").count(),
            1
        );
        assert_eq!(manifest.files.iter().filter(|f| f.kind == "crt").count(), 1);
        assert_eq!(
            manifest.files.iter().filter(|f| f.kind == "wheel").count(),
            24
        );
        assert!(
            manifest.files.iter().map(|f| f.size).sum::<u64>()
                <= package("whisper").unwrap().max_download
        );
    }
    #[test]
    fn archive_paths_and_receipts_cannot_escape_or_alias_windows_files() {
        for name in [
            "../x.exe",
            "/x.exe",
            "C:/x.exe",
            "bin\\x.exe",
            "bin/x.exe:evil",
            "bin/NUL.txt",
            "bin/COM1.exe",
            "bin/a. ",
            "bin//x",
            "a/../b",
        ] {
            assert!(safe_name(name).is_err(), "{name}");
        }
        assert_eq!(
            safe_name("root/bin/speech.exe").unwrap(),
            PathBuf::from("root/bin/speech.exe")
        );
        assert!(package("https://elsewhere.example/x.zip").is_err());
        assert!(check_digest(&[0u8; 32], PACKAGES[0].sha256).is_err());
        assert!(managed_program(Path::new("."), "speech", "other").is_none());
    }
    #[test]
    fn extraction_rejects_traversal_case_duplicates_links_and_cancel() {
        use zip::write::SimpleFileOptions;
        let root = std::env::temp_dir().join(format!(
            "transcription-runtime-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&root).unwrap();
        for (index, names) in [["../escape", "ok"], ["Bin/a.exe", "bin/A.exe"]]
            .iter()
            .enumerate()
        {
            let zip_path = root.join(format!("bad{index}.zip"));
            let dest = root.join(format!("out{index}"));
            fs::create_dir(&dest).unwrap();
            let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            for name in names {
                zip.start_file(*name, SimpleFileOptions::default()).unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
            assert!(extract(&zip_path, &dest, &CancellationToken::new()).is_err());
        }
        let zip_path = root.join("cancel.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        zip.start_file("safe.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"data").unwrap();
        zip.finish().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(extract(&zip_path, &root, &token).is_err());
        assert!(!root.join("safe.txt").exists());
        assert!(!root.parent().unwrap().join("escape").exists());
        let valid = root.join("valid");
        fs::create_dir(&valid).unwrap();
        extract(&zip_path, &valid, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(valid.join("safe.txt")).unwrap(), b"data");
        let link_zip = root.join("link.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&link_zip).unwrap());
        zip.add_symlink("link", "../outside", SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(extract(&link_zip, &valid, &CancellationToken::new()).is_err());
        assert!(!valid.join("link").exists());
        let oversized = root.join("oversized.zip");
        let mut bytes = fs::read(&zip_path).unwrap();
        let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&((MAX_UNPACK + 1) as u32).to_le_bytes());
        fs::write(&oversized, bytes).unwrap();
        assert!(extract(&oversized, &valid, &CancellationToken::new()).is_err());
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
