//! Only fixed model caches are deletable. No client-supplied path is accepted.
use crate::catalog::Tools;
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn environment(tools: &Tools, name: &str) -> Option<PathBuf> {
    tools
        .environment
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| std::env::var_os(name).map(PathBuf::from))
}
fn whisper_cache(tools: &Tools) -> Result<PathBuf> {
    let (_, args) = tools
        .whisper
        .as_ref()
        .context("Whisperの保存先を確認できません。実行環境を先に導入してください。")?;
    if let Some(index) = args.iter().position(|a| a == "--model-dir") {
        return args
            .get(index + 1)
            .map(PathBuf::from)
            .context("Whisperの保存先が不正です");
    }
    // Match huggingface_hub's cache precedence, including legacy HUGGINGFACE_HUB_CACHE.
    if let Some(path) =
        environment(tools, "HF_HUB_CACHE").or_else(|| environment(tools, "HUGGINGFACE_HUB_CACHE"))
    {
        return Ok(path);
    }
    if let Some(path) = environment(tools, "HF_HOME") {
        return Ok(path.join("hub"));
    }
    if let Some(path) = environment(tools, "XDG_CACHE_HOME") {
        return Ok(path.join("huggingface/hub"));
    }
    let home = environment(tools, "USERPROFILE")
        .or_else(|| environment(tools, "HOME"))
        .context("Whisperのキャッシュ保存先を確認できません")?;
    Ok(home.join(".cache/huggingface/hub"))
}
fn repository(model: &str) -> Result<String> {
    if !crate::catalog::WHISPER_MODELS
        .iter()
        .any(|row| row.0 == model)
    {
        bail!("管理対象外のWhisperモデルです")
    }
    Ok(if model == "large-v3-turbo" {
        "models--mobiuslabsgmbh--faster-whisper-large-v3-turbo".into()
    } else {
        format!("models--Systran--faster-whisper-{model}")
    })
}
/// Validate the exact direct child before a recursive delete. Never traverse a linked directory.
fn delete_repository(root: &Path, name: &str) -> Result<()> {
    if !name.starts_with("models--") || name.contains(['/', '\\', ':']) {
        bail!("モデル保存先が不正です")
    }
    let base = root
        .canonicalize()
        .context("モデルの保存先が見つかりません")?;
    let target = base.join(name);
    let metadata = fs::symlink_metadata(&target).context("モデルの保存ファイルが見つかりません")?;
    if !metadata.is_dir()
        || linked(&metadata)
        || target.canonicalize()?.parent() != Some(base.as_path())
    {
        bail!("モデル保存先が通常の管理フォルダーではありません。削除しません。")
    }
    let mut directories = vec![target.clone()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                if linked(&metadata) {
                    bail!("モデル内にリンクされたフォルダーがあります。削除しません。")
                }
                directories.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(&target)
        .context("モデルを削除できませんでした。使用中でないか確認してください")
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
pub fn delete_whisper(tools: &Tools, model: &str) -> Result<()> {
    delete_repository(&whisper_cache(tools)?, &repository(model)?)
}
pub fn delete_nemo(tools: &Tools) -> Result<()> {
    let path = &tools.nemo_model;
    if path.file_name().and_then(|n| n.to_str()) != Some(crate::nemo::FILENAME) {
        bail!("Nemotron ASRの保存先が不正です")
    }
    let metadata =
        fs::symlink_metadata(path).context("Nemotron ASRの保存ファイルが見つかりません")?;
    if !metadata.is_file() || linked(&metadata) {
        bail!("Nemotron ASRが通常の保存ファイルではありません")
    }
    fs::remove_file(path).context("Nemotron ASRを削除できませんでした")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_whisper_cache_and_nemo_deletion_keep_other_files() {
        let root = std::env::temp_dir().join(format!("tc-model-cache-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut tools = Tools {
            environment: vec![(
                "HF_HUB_CACHE".into(),
                root.join("inherited").to_string_lossy().into_owned(),
            )],
            ffmpeg: None,
            ffprobe: None,
            speech: None,
            nemo: None,
            nemo_model: root.join(crate::nemo::FILENAME),
            whisper: Some((
                "fixture.exe".into(),
                vec![
                    "--model-dir".into(),
                    root.join("explicit").to_string_lossy().into_owned(),
                ],
            )),
        };
        assert_eq!(whisper_cache(&tools).unwrap(), root.join("explicit"));
        tools.whisper.as_mut().unwrap().1.clear();
        assert_eq!(whisper_cache(&tools).unwrap(), root.join("inherited"));
        fs::write(&tools.nemo_model, b"fixture").unwrap();
        fs::write(root.join("other.gguf"), b"keep").unwrap();
        delete_nemo(&tools).unwrap();
        assert!(!tools.nemo_model.exists());
        assert!(root.join("other.gguf").is_file());
        tools.nemo_model = root.join("other.gguf");
        assert!(delete_nemo(&tools).is_err());
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn deletion_removes_only_selected_repository_and_rejects_escape() {
        let root = std::env::temp_dir().join(format!("tc-model-delete-{}", uuid::Uuid::new_v4()));
        let name = repository("tiny").unwrap();
        fs::create_dir_all(root.join(&name).join("snapshots/revision")).unwrap();
        fs::write(
            root.join(&name).join("snapshots/revision/model.bin"),
            b"fixture",
        )
        .unwrap();
        fs::create_dir(root.join(repository("base").unwrap())).unwrap();
        fs::write(root.join("recording.wav"), b"keep").unwrap();
        assert!(repository("../../base").is_err());
        assert!(delete_repository(&root, "models--../other").is_err());
        assert!(delete_repository(&root, "recording.wav").is_err());
        delete_repository(&root, &name).unwrap();
        assert!(!root.join(name).exists());
        assert!(root.join(repository("base").unwrap()).is_dir());
        assert_eq!(fs::read(root.join("recording.wav")).unwrap(), b"keep");
        assert!(
            root.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
