//! Runtime file sizes only: model/cache/backup data and linked directories are excluded.
use crate::{app::App, runtime_install};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// Fixed package reference measurements, not the capacity of an existing external installation.
const REFERENCE_INSTALLED: [u64; 4] = [299_960_008, 92_443_661, 77_063_122, 318_594_126];
const REFERENCE_DOWNLOAD: [u64; 4] = [125_450_083, 29_548_632, 22_417_032, 109_728_040];
struct Plan {
    id: &'static str,
    roots: Vec<PathBuf>,
    external: bool,
    installed_reference: u64,
    download_reference: u64,
}

fn plans(app: &App) -> Vec<Plan> {
    let tools = app.tools();
    runtime_install::PACKAGES
        .iter()
        .enumerate()
        .map(|(index, p)| {
            let managed = runtime_install::target(&app.runtime.home, p);
            let mut roots = vec![];
            if managed.exists() {
                roots.push(managed.clone());
            }
            let paths: Vec<&Path> = match p.id {
                "whisper" => tools
                    .whisper
                    .as_ref()
                    .map(|v| v.0.as_path())
                    .into_iter()
                    .collect(),
                "speech" => tools.speech.as_deref().into_iter().collect(),
                "nemo" => tools.nemo.as_deref().into_iter().collect(),
                _ => [tools.ffmpeg.as_deref(), tools.ffprobe.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect(),
            };
            let bundled = if p.id == "whisper" {
                app.runtime.root.join("worker")
            } else {
                app.runtime.root.join("runtime").join(if p.id == "nemo" {
                    "nemo-speech"
                } else {
                    p.id
                })
            };
            let mut external = false;
            for path in paths {
                if within(path, &managed) {
                    continue;
                }
                if within(path, &bundled) {
                    roots.push(bundled.clone());
                } else {
                    external = true;
                } // Do not scan arbitrary configured folders such as System32.
            }
            Plan {
                id: p.id,
                roots,
                external,
                installed_reference: REFERENCE_INSTALLED[index],
                download_reference: REFERENCE_DOWNLOAD[index],
            }
        })
        .collect()
}
fn within(path: &Path, root: &Path) -> bool {
    path.canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(p, r)| p.starts_with(r))
}
fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn excluded(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "models" | "cache" | ".cache" | "__pycache__" | "backups"
    ) || name.starts_with("models--")
        || name.starts_with(".previous-")
        || name.ends_with(".gguf")
        || name.ends_with(".safetensors")
        || name.ends_with(".weights")
        || name == "model.bin"
}
fn scan(roots: Vec<PathBuf>, seen: &mut HashSet<PathBuf>) -> (u64, bool) {
    let mut todo = roots;
    let mut bytes = 0u64;
    let mut complete = true;
    let mut count = 0;
    while let Some(path) = todo.pop() {
        count += 1;
        if count > 100_000 {
            complete = false;
            break;
        }
        if excluded(&path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            complete = false;
            continue;
        };
        if linked(&metadata) {
            complete = false;
            continue;
        }
        let Ok(canonical) = path.canonicalize() else {
            complete = false;
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            match fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => todo.push(entry.path()),
                            Err(_) => complete = false,
                        }
                    }
                }
                Err(_) => complete = false,
            }
        }
    }
    (bytes, complete)
}
fn measure(plans: Vec<Plan>) -> Value {
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut unmeasured = 0;
    let rows:Vec<_>=plans.into_iter().map(|p| {
        let (bytes,scanned)=scan(p.roots,&mut seen);
        let complete=scanned&&!p.external;
        total+=bytes;
        if !complete { unmeasured+=1; }
        json!({"id":p.id,"measuredBytes":bytes,"capacityComplete":complete,"external":p.external,
            "referenceInstalledBytes":p.installed_reference,"referenceDownloadBytes":p.download_reference})
    }).collect();
    json!({"runtimes":rows,"totalBytes":total,"complete":unmeasured==0,"unmeasuredRuntimeCount":unmeasured,
        "referenceInstalledBytes":REFERENCE_INSTALLED.iter().sum::<u64>(),"referenceDownloadBytes":REFERENCE_DOWNLOAD.iter().sum::<u64>(),
        "basis":"logical_files_excluding_models_cache_backups","measuredAt":crate::store::now()})
}
pub async fn snapshot(app: &App) -> Value {
    if let Some((at, value)) = &*app.runtime_storage.lock().unwrap()
        && at.elapsed() < Duration::from_secs(30)
    {
        return value.clone();
    }
    let plan = plans(app);
    let value = tokio::task::spawn_blocking(move || measure(plan))
        .await
        .unwrap_or_else(|_| json!({"error":"容量を集計できませんでした。"}));
    *app.runtime_storage.lock().unwrap() = Some((Instant::now(), value.clone()));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_excludes_models_backups_and_links_and_deduplicates_roots() {
        let root = std::env::temp_dir().join(format!("tc-runtime-size-{}", uuid::Uuid::new_v4()));
        for name in [
            "current/lib",
            "current/models",
            "current/cache",
            ".previous-old",
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        for (path, size) in [
            ("current/runtime.exe", 17),
            ("current/lib/dependency.dll", 23),
            ("current/models/model.bin", 99),
            ("current/cache/data", 99),
            ("current/weights.gguf", 99),
            (".previous-old/lib.dll", 99),
        ] {
            fs::write(root.join(path), vec![0; size]).unwrap();
        }
        let (bytes, complete) = scan(
            vec![root.clone(), root.join("current/lib")],
            &mut HashSet::new(),
        );
        assert!(complete);
        assert_eq!(bytes, 40);
        let value = measure(vec![Plan {
            id: "external",
            roots: vec![root.clone()],
            external: true,
            installed_reference: 100,
            download_reference: 50,
        }]);
        assert_eq!(value["totalBytes"], 40);
        assert_eq!(value["complete"], false);
        assert_eq!(value["unmeasuredRuntimeCount"], 1);
        #[cfg(windows)]
        {
            // A junction outside the runtime root must never contribute to the total.
            let script = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' -ErrorAction Stop | Out-Null",
                root.join("current/link")
                    .to_string_lossy()
                    .replace('\'', "''"),
                root.join("current/models")
                    .to_string_lossy()
                    .replace('\'', "''")
            );
            let output = std::process::Command::new("pwsh")
                .args(["-NoProfile", "-Command", &script])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            {
                let (bytes, complete) = scan(vec![root.clone()], &mut HashSet::new());
                assert_eq!(bytes, 40);
                assert!(!complete);
                fs::remove_dir(root.join("current/link")).unwrap();
            }
        }
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
