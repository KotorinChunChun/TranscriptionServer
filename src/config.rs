use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_ID: &str = "{6C0A7E36-24C5-4D67-8D18-0677E953741B}";
#[derive(Parser, Debug)]
#[command(version, about = "TranscriptionServer — LAN文字起こしサーバー")]
pub struct Args {
    /// Skip automatic startup preparation (offline/development use).
    #[arg(long)]
    pub no_auto_runtimes: bool,
    #[arg(long)]
    pub enable_integrated_model: bool,
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
    #[arg(long)]
    pub https_only: bool,
    /// Stop the server first; inspect existing uploaded media without starting HTTP.
    #[arg(long, requires = "data_dir")]
    pub compact_media: bool,
    /// Apply offline media compaction and backfill legacy fingerprints.
    #[arg(long, requires = "compact_media")]
    pub apply: bool,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long, conflicts_with_all=["lan","local"])]
    pub server: bool,
    #[arg(long, conflicts_with = "local")]
    pub lan: bool,
    #[arg(long)]
    pub local: bool,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub speech: Option<PathBuf>,
    #[arg(long)]
    pub nemo_speech: Option<PathBuf>,
    #[arg(long)]
    pub ffmpeg: Option<PathBuf>,
    #[arg(long)]
    pub ffprobe: Option<PathBuf>,
    #[arg(long)]
    pub whisper_worker: Option<PathBuf>,
    #[arg(long)]
    pub whisper_python: Option<PathBuf>,
    #[arg(long)]
    pub threads: Option<u16>,
    #[arg(long)]
    pub allow_origin: Vec<String>,
    #[arg(long)]
    pub allow_network: Vec<String>,
    #[arg(long)]
    pub installed: bool,
    #[arg(long)]
    pub settings_dir: Option<PathBuf>,
    #[arg(long)]
    pub process_record: Option<PathBuf>,
    #[arg(long)]
    pub instance_id: Option<String>,
    #[arg(long)]
    pub open: bool,
    /// Open the client connection guide after startup or in an existing instance.
    #[arg(long)]
    pub connection_guide: bool,
    #[arg(long)]
    pub no_browser: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub execution: crate::scheduler::Settings,
    pub auto_install_runtimes: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub https_only: bool,
    pub instance_id: String,
    pub host: String,
    pub port: u16,
    pub server: bool,
    pub lan: bool,
    pub data_dir: PathBuf,
    pub speech: Option<PathBuf>,
    pub nemo_speech: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub whisper_worker: Option<PathBuf>,
    pub whisper_python: Option<PathBuf>,
    pub threads: Option<u16>,
    pub allow_origin: Vec<String>,
    pub enable_integrated_model: bool,
    pub allow_network: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            execution: crate::scheduler::Settings::default(),
            auto_install_runtimes: true,
            tls_cert: None,
            tls_key: None,
            https_only: false,
            instance_id: "shared".into(),
            host: "0.0.0.0".into(),
            port: 8769,
            server: false,
            lan: true,
            data_dir: user_root().join("data"),
            speech: None,
            nemo_speech: None,
            ffmpeg: None,
            ffprobe: None,
            whisper_worker: None,
            whisper_python: None,
            threads: None,
            enable_integrated_model: false,
            allow_origin: vec![],
            allow_network: vec![],
        }
    }
}
pub struct Runtime {
    pub config: Config,
    pub root: PathBuf,
    pub home: PathBuf,
    pub installed: bool,
    pub process_record: Option<PathBuf>,
    pub open: bool,
}
pub fn user_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("TranscriptionServer")
}
pub fn friendly(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{rest}"))
    } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}
pub fn installed(root: &Path) -> bool {
    read_json(&root.join("INSTALLATION.json"))
        .map(|v| {
            v["product"] == "TranscriptionServer" && v["appId"] == APP_ID && v["scope"] == "user"
        })
        .unwrap_or(false)
}
pub fn read_json(path: &Path) -> Result<serde_json::Value> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(
        bytes.strip_prefix(&[239, 187, 191]).unwrap_or(&bytes),
    )?)
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("保存先が不正です")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        use std::io::Write;
        let mut file = fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

impl Runtime {
    pub fn load(args: Args) -> Result<Self> {
        let private_instance = args.instance_id.as_deref().is_some_and(|v| v != "shared");
        if let Some(id) = &args.instance_id
            && (id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')))
        {
            bail!("instance-idは英数字と._-の1〜64文字です")
        }
        if private_instance
            && (!args.local
                || args.installed
                || args.port.is_none_or(|p| p == 0 || p == 8769)
                || args.data_dir.as_ref().is_none_or(|p| !p.is_absolute())
                || args.settings_dir.as_ref().is_none_or(|p| !p.is_absolute())
                || args
                    .process_record
                    .as_ref()
                    .is_none_or(|p| !p.is_absolute()))
        {
            bail!(
                "個別インスタンスは--local、専用port（8769以外）、絶対パスのdata-dir/settings-dir/process-recordが必要です。--installedは指定しません。"
            )
        }
        let root = std::env::current_exe()?
            .parent()
            .context("EXEの場所が不明です")?
            .to_path_buf();
        let is_installed = installed(&root) && !private_instance;
        if args.installed && !is_installed {
            bail!("インストーラー版を指定してください。")
        }
        let home = args.settings_dir.unwrap_or_else(user_root);
        let settings = home.join("settings.json");
        let raw = if settings.exists() && (is_installed || private_instance) {
            read_json(&settings)?
        } else {
            serde_json::json!({})
        };
        let mut cfg: Config =
            serde_json::from_value(raw.clone()).context("保存した設定が不正です")?;
        if let Some(id) = args.instance_id {
            cfg.instance_id = id;
        }
        // Old loopback-only settings are explicitly migrated to the newly
        // authorized LAN default, while configured Bearer deployments stay so.
        if cfg.server {
            cfg.lan = false;
        } else if raw.get("lan").is_none() {
            cfg.lan = true;
            cfg.host = "0.0.0.0".into();
        }
        let legacy = user_root()
            .parent()
            .unwrap_or(Path::new("."))
            .join("HaasMeetingWorkbench");
        let default_data = user_root().join("data");
        let selected_data = args.data_dir.as_ref().unwrap_or(&cfg.data_dir);
        let legacy_selected = selected_data
            .to_string_lossy()
            .eq_ignore_ascii_case(&legacy.to_string_lossy());
        let implicit_legacy = args.data_dir.is_none()
            && raw.get("data_dir").is_none()
            && !default_data.join("workbench.sqlite3").exists();
        if private_instance && legacy_selected {
            bail!("個別インスタンスに旧共有データの保存先は指定できません")
        }
        if !private_instance
            && (legacy_selected || implicit_legacy)
            && legacy.join("workbench.sqlite3").exists()
        {
            let backup = home.join("settings-before-v0.2-data-migration.json");
            if settings.exists() && !backup.exists() {
                fs::copy(&settings, &backup)?;
            }
            crate::migration::migrate(&legacy, &default_data)?;
            cfg.data_dir = default_data;
        }
        if args.server {
            cfg.server = true;
            cfg.lan = false
        }
        if args.lan {
            cfg.server = false;
            cfg.lan = true
        }
        if args.local {
            cfg.server = false;
            cfg.lan = false;
            cfg.host = "127.0.0.1".into()
        }
        if let Some(v) = args.host {
            cfg.host = v
        }
        if let Some(v) = args.port {
            cfg.port = v
        }
        if let Some(v) = args.data_dir.filter(|v| {
            !v.to_string_lossy()
                .eq_ignore_ascii_case(&legacy.to_string_lossy())
        }) {
            cfg.data_dir = v
        }
        if let Some(v) = args.speech {
            cfg.speech = Some(v)
        }
        if let Some(v) = args.nemo_speech {
            cfg.nemo_speech = Some(v)
        }
        if let Some(v) = args.ffmpeg {
            cfg.ffmpeg = Some(v)
        }
        if let Some(v) = args.ffprobe {
            cfg.ffprobe = Some(v)
        }
        if let Some(v) = args.whisper_worker {
            cfg.whisper_worker = Some(v)
        }
        if args.no_auto_runtimes {
            cfg.auto_install_runtimes = false;
        }
        if args.enable_integrated_model {
            cfg.enable_integrated_model = true;
        }
        if let Some(v) = args.tls_cert {
            cfg.tls_cert = Some(v);
        }
        if let Some(v) = args.tls_key {
            cfg.tls_key = Some(v);
        }
        if args.https_only {
            cfg.https_only = true;
        }
        if cfg.tls_cert.is_some() != cfg.tls_key.is_some()
            || (cfg.https_only && cfg.tls_cert.is_none())
        {
            bail!("TLS証明書と秘密鍵は両方指定してください。https-onlyにはTLS設定が必要です。")
        }
        for path in [&cfg.tls_cert, &cfg.tls_key].into_iter().flatten() {
            if !path.is_absolute() {
                bail!("TLS証明書と秘密鍵は絶対パスで指定してください。")
            }
        }
        if let Some(v) = args.whisper_python {
            cfg.whisper_python = Some(v)
        }
        if let Some(v) = args.threads {
            cfg.threads = Some(v)
        }
        if !args.allow_origin.is_empty() {
            cfg.allow_origin = args.allow_origin
        }
        if !args.allow_network.is_empty() {
            cfg.allow_network = args.allow_network
        }
        if cfg.port == 0 || cfg.threads == Some(0) {
            bail!("port/threadsが不正です")
        }
        let ip: std::net::IpAddr = cfg
            .host
            .parse()
            .or_else(|_| {
                if cfg.host == "localhost" {
                    Ok(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
                } else {
                    Err(())
                }
            })
            .map_err(|_| anyhow::anyhow!("hostはIPアドレスを指定してください"))?;
        if !cfg.server && !cfg.lan && !ip.is_loopback() {
            bail!("LAN待受は--lan、外部公開は--serverを指定してください")
        }
        if !cfg.data_dir.is_absolute() {
            cfg.data_dir = std::env::current_dir()?.join(&cfg.data_dir)
        }
        fs::create_dir_all(&cfg.data_dir)?;
        cfg.data_dir = fs::canonicalize(&cfg.data_dir)?;
        if is_installed && cfg.data_dir.starts_with(fs::canonicalize(&root)?) {
            bail!("データはプログラムの外へ保存してください")
        }
        cfg.data_dir = friendly(&cfg.data_dir);
        if private_instance {
            let shared_home = fs::canonicalize(user_root()).unwrap_or_else(|_| user_root());
            let shared_data = fs::canonicalize(user_root().join("data"))
                .unwrap_or_else(|_| user_root().join("data"));
            let selected_home = fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
            if friendly(&shared_home)
                .to_string_lossy()
                .eq_ignore_ascii_case(&friendly(&selected_home).to_string_lossy())
                || friendly(&shared_data)
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&cfg.data_dir.to_string_lossy())
            {
                bail!("個別インスタンスには共有サーバーと異なる保存先・設定先を指定してください")
            }
        }
        Ok(Self {
            config: cfg,
            root,
            home,
            installed: is_installed,
            process_record: args.process_record,
            open: (args.open || args.connection_guide) && !args.no_browser,
        })
    }
    pub fn save(&self) -> Result<()> {
        if self.installed {
            let path = self.home.join("settings.json");
            let mut document = if path.exists() {
                read_json(&path)?
            } else {
                serde_json::json!({})
            };
            let object = document
                .as_object_mut()
                .context("設定JSONはオブジェクトである必要があります")?;
            object.extend(
                serde_json::to_value(&self.config)?
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            write_json(&path, &document)?;
        }
        Ok(())
    }
}
pub fn program(explicit: &Option<PathBuf>, root: &Path, name: &str) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.clone());
    }
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    for path in [
        root.join("bin").join(&filename),
        root.join("runtime").join(name).join(&filename),
        root.join("runtime/ffmpeg/bin").join(&filename),
        root.join("runtime/ffmpeg").join(&filename),
    ] {
        if path.is_file() {
            return Some(path);
        }
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(&filename))
        .find(|p| p.is_file())
}
#[cfg(test)]
mod instance_tests {
    use super::*;
    #[test]
    fn private_instance_requires_explicit_local_ownership_and_keeps_shared_defaults_out() {
        for args in [
            vec!["server", "--instance-id", "transcript-cutter"],
            vec!["server", "--instance-id", "../bad"],
            vec!["server", "--instance-id", "transcript-cutter", "--lan"],
            vec![
                "server",
                "--instance-id",
                "transcript-cutter",
                "--local",
                "--port",
                "8769",
            ],
        ] {
            assert!(Runtime::load(Args::try_parse_from(args).unwrap()).is_err());
        }
        let root = std::env::temp_dir().join(format!("tc-private-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let settings = root.join("settings");
        let record = root.join("process.json");
        let args = Args::try_parse_from([
            "server",
            "--instance-id",
            "transcript-cutter",
            "--local",
            "--port",
            "48055",
            "--data-dir",
            data.to_str().unwrap(),
            "--settings-dir",
            settings.to_str().unwrap(),
            "--process-record",
            record.to_str().unwrap(),
        ])
        .unwrap();
        let runtime = Runtime::load(args).unwrap();
        assert_eq!(runtime.config.instance_id, "transcript-cutter");
        assert!(!runtime.config.lan);
        assert!(!runtime.config.server);
        assert!(!runtime.installed);
        assert_eq!(
            runtime.config.data_dir,
            friendly(&fs::canonicalize(&data).unwrap())
        );
        assert_eq!(runtime.home, settings);
        assert_eq!(runtime.process_record, Some(record));
        runtime.save().unwrap();
        assert!(!runtime.home.join("settings.json").exists());
        for suppress in [false, true] {
            let guide_record = root.join("guide-process.json");
            let mut cli = vec![
                "server",
                "--instance-id",
                "guide-fixture",
                "--local",
                "--port",
                "48055",
                "--settings-dir",
                settings.to_str().unwrap(),
                "--data-dir",
                data.to_str().unwrap(),
                "--process-record",
                guide_record.to_str().unwrap(),
                "--connection-guide",
            ];
            if suppress {
                cli.push("--no-browser");
            }
            let args = Args::try_parse_from(cli).unwrap();
            assert!(args.connection_guide);
            let guide = Runtime::load(args).unwrap();
            assert_eq!(guide.open, !suppress);
        }
        assert!(root.starts_with(std::env::temp_dir()));
        fs::remove_dir_all(root).unwrap();
    }
}
