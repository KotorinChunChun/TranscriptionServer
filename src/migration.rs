use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

fn linked(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(meta.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(meta.file_type().is_symlink())
    }
}
fn digest(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().to_vec())
}
fn copy(source: &Path, target: &Path, root: bool) -> Result<()> {
    if linked(source)? {
        bail!("データ内のリンクを検出しました。旧保存先を保持して移行を中止します。")
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if root
            && matches!(
                name.to_str(),
                Some(
                    "workbench.sqlite3"
                        | "workbench.sqlite3-wal"
                        | "workbench.sqlite3-shm"
                        | ".service.lock"
                )
            )
        {
            continue;
        }
        let path = entry.path();
        if linked(&path)? {
            bail!("データ内のリンクを検出しました。旧保存先は保持しています。")
        }
        let dest = target.join(name);
        if path.is_dir() {
            copy(&path, &dest, false)?
        } else {
            fs::copy(&path, &dest)?;
            if digest(&path)? != digest(&dest)? {
                bail!("データコピーの照合に失敗しました")
            }
        }
    }
    Ok(())
}
/// Copy and commit a consistent snapshot. Never delete or modify the legacy DB/media.
pub fn migrate(legacy: &Path, target: &Path) -> Result<Value> {
    if target.join("workbench.sqlite3").is_file()
        && let Ok(receipt) = crate::config::read_json(&target.join("migration.json"))
        && receipt["completed"] == true
        && receipt["from"] == crate::config::friendly(legacy).to_string_lossy().as_ref()
        && receipt["to"] == crate::config::friendly(target).to_string_lossy().as_ref()
    {
        return Ok(receipt);
    }
    if linked(legacy)? {
        bail!("旧保存先がリンクです。移行を中止しました。")
    }
    let parent = target.parent().context("新保存先が不正です")?;
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".data-migration.lock"))?;
    lock.try_lock_exclusive()
        .context("データ移行は別プロセスが実行中です")?;
    if target.exists() && (linked(target)? || fs::read_dir(target)?.next().is_some()) {
        bail!(
            "新旧の保存先が両方存在します。自動上書き・統合は行いません。旧データは保持しています。"
        )
    }
    let old_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(legacy.join(".service.lock"))?;
    old_lock
        .try_lock_exclusive()
        .context("旧サーバーが保存先を使用中です。正常終了してから起動してください")?;
    let db = Connection::open_with_flags(
        legacy.join("workbench.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let stage = parent.join(format!(".data-migration-{}", uuid::Uuid::new_v4().simple()));
    if fs2::available_space(parent)? < crate::store::tree_size(legacy)? + 64 * 1024 * 1024 {
        bail!("データ移行の空き容量が不足しています。旧データは保持しています。")
    }
    copy(legacy, &stage, true)?;
    db.backup(
        "main",
        stage.join("workbench.sqlite3"),
        rusqlite::backup::NO_PROGRESS,
    )?;
    let snapshot = Connection::open(stage.join("workbench.sqlite3"))?;
    let check: String = snapshot.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        bail!("SQLiteの移行照合に失敗しました。旧データは保持しています。")
    }
    for table in ["jobs", "segments"] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let before: i64 = db.query_row(&sql, [], |r| r.get(0))?;
        let after: i64 = snapshot.query_row(&sql, [], |r| r.get(0))?;
        if before != after {
            bail!("SQLiteの件数が一致しません")
        }
    }
    drop(snapshot);
    drop(db);
    let receipt = json!({"completed":true,"from":crate::config::friendly(legacy),"to":crate::config::friendly(target),"legacyRetained":true,"version":crate::config::VERSION});
    crate::config::write_json(&stage.join("migration.json"), &receipt)?;
    if target.exists() {
        fs::remove_dir(target)?;
    }
    fs::rename(&stage, target)
        .context("新保存先への確定に失敗しました。旧データと移行作業フォルダーは保持しています")?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("ts-migrate-{}", uuid::Uuid::new_v4()));
        let old = base.join("HaasMeetingWorkbench");
        let new = base.join("TranscriptionServer/data");
        fs::create_dir_all(&old).unwrap();
        let db = Connection::open(old.join("workbench.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE jobs(id TEXT PRIMARY KEY,payload TEXT);CREATE TABLE segments(job_id TEXT,id INTEGER,payload TEXT);INSERT INTO jobs VALUES('original','rich payload');INSERT INTO segments VALUES('original',1,'original caption');").unwrap();
        fs::create_dir(old.join("original")).unwrap();
        fs::write(
            old.join("original/source.media"),
            b"retained original media",
        )
        .unwrap();
        (base, old, new)
    }
    #[test]
    fn preserves_database_media_and_legacy() {
        let (base, old, new) = fixture();
        assert!(migrate(&old, &new).unwrap()["legacyRetained"] == true);
        assert_eq!(
            fs::read(new.join("original/source.media")).unwrap(),
            fs::read(old.join("original/source.media")).unwrap()
        );
        let db = Connection::open(new.join("workbench.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT payload FROM segments", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "original caption"
        );
        db.execute("INSERT INTO jobs VALUES('after-migration','new data')", [])
            .unwrap();
        drop(db);
        migrate(&old, &new).unwrap();
        let db = Connection::open(new.join("workbench.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(db);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn refuses_busy_source_and_nonempty_target() {
        let (base, old, new) = fixture();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(old.join(".service.lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();
        assert!(migrate(&old, &new).is_err());
        drop(lock);
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("do-not-overwrite"), b"existing").unwrap();
        assert!(migrate(&old, &new).is_err());
        assert_eq!(fs::read(new.join("do-not-overwrite")).unwrap(), b"existing");
        fs::remove_dir_all(base).unwrap();
    }
}
