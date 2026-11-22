use crate::store::Store;
use rusqlite::{Connection, backup::NO_PROGRESS, params};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

#[test]
fn unsigned_sql_values_keep_checked_integer_limits() {
    let db = Connection::open_in_memory().unwrap();
    let largest = i64::MAX as u64;
    assert_eq!(
        db.query_row("SELECT ?", [largest], |r| r.get::<_, u64>(0))
            .unwrap(),
        largest
    );
    assert!(
        db.query_row("SELECT ?", [u64::MAX], |r| r.get::<_, i64>(0))
            .is_err()
    );
    assert!(
        db.query_row("SELECT -1", [], |r| r.get::<_, u64>(0))
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT ?", [25_usize], |r| r.get::<_, usize>(0))
            .unwrap(),
        25
    );
}

#[test]
fn bundled_sqlite_backup_restore_preserves_json_and_transaction() {
    assert_eq!(rusqlite::version(), "3.54.1");
    let root = std::env::temp_dir().join(format!("tc-sqlite-backup-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let mut db = Connection::open(root.join("source.sqlite3")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(id INTEGER PRIMARY KEY,payload TEXT NOT NULL);").unwrap();
    let payload =
        json!({"text":"合成字幕","model":"whisper/base","unknown":[1,null,{"extra":"保持"}]});
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO fixture VALUES(?,?)",
        params![i64::MAX, payload.to_string()],
    )
    .unwrap();
    tx.commit().unwrap();
    {
        let tx = db.transaction().unwrap();
        tx.execute("UPDATE fixture SET payload='{}'", []).unwrap();
        tx.rollback().unwrap();
    }
    let backup = root.join("backup.sqlite3");
    db.backup("main", &backup, NO_PROGRESS).unwrap();
    let mut restored = Connection::open(root.join("restored.sqlite3")).unwrap();
    restored
        .execute_batch("CREATE TABLE should_be_replaced(id INTEGER);")
        .unwrap();
    restored.restore("main", &backup, NO_PROGRESS).unwrap();
    let (id, raw): (i64, String) = restored
        .query_row("SELECT id,payload FROM fixture", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(id, i64::MAX);
    assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), payload);
    assert_eq!(
        restored
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        restored
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='should_be_replaced'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(restored);
    drop(db);
    fs::remove_dir_all(root).unwrap();
}

/// 旧rusqliteで生成した通常のDB/WALを入力とする、明示実行の互換試験。
#[test]
#[ignore = "旧SQLiteの隔離合成DBをTRANSCRIPTION_SQLITE_FIXTUREで指定して実行"]
fn legacy_wal_store_backup_restore_roundtrip() {
    let root =
        PathBuf::from(std::env::var_os("TRANSCRIPTION_SQLITE_FIXTURE").expect("隔離合成DBのパス"));
    assert!(root.is_absolute());
    let fixture: Value =
        serde_json::from_slice(&fs::read(root.join("fixture-only.json")).unwrap()).unwrap();
    assert_eq!(fixture["purpose"], "transcription-synthetic-sqlite");
    assert!(fixture["sqliteNumber"].as_i64().unwrap() < 3_054_000);
    let id = fixture["id"].as_str().unwrap();
    let wal = fs::read(root.join("workbench.sqlite3-wal")).unwrap();
    let header = fs::read(root.join("workbench.sqlite3")).unwrap();
    assert!(wal.len() > 32);
    assert_eq!(
        u16::from_be_bytes([header[16], header[17]]) as u32,
        u32::from_be_bytes(wal[8..12].try_into().unwrap())
    );
    let audio_path = root.join(id).join("source.media");
    let audio = fs::read(&audio_path).unwrap();
    let store = Store::open(&root).unwrap();
    let old_job = store.get(id).unwrap();
    assert_eq!(old_job["unknown"], fixture["job"]["unknown"]);
    assert_eq!(
        old_job["metadata"]["oldExtra"],
        fixture["job"]["metadata"]["oldExtra"]
    );
    assert_eq!(store.page(25, 0, None, None).unwrap()["total"], 1);
    assert_eq!(
        store.segments(id, 0, 200).unwrap()["segments"][0],
        fixture["segment"]
    );
    store
        .edit(id, &[json!({"id":1,"text":"新版で編集した合成字幕"})])
        .unwrap();
    let job = store
        .create(
            json!({"status":"running","segmentCount":0,"processedUntil":0,
        "engine":"whisper","modelId":"base","clientId":"sqlite-compat-fixture","workload":"file",
        "metadata":{"compatibility":"new-write"}}),
        )
        .unwrap();
    let new_id = job["id"].as_str().unwrap();
    store
        .append_many(
            new_id,
            vec![
                json!({"start":0.0,"end":1.0,"text":"新規の合成字幕1"}),
                json!({"start":1.0,"end":2.0,"text":"新規の合成字幕2"}),
            ],
        )
        .unwrap();
    store
        .update(new_id, json!({"status":"complete","phase":"complete"}))
        .unwrap();
    assert_eq!(
        store.page(1, 1, None, None).unwrap()["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.segments(new_id, 1, 1).unwrap()["segments"][0]["id"],
        2
    );
    let expected_job = store.get(new_id).unwrap();
    let expected_segments = store.segments(new_id, 0, 200).unwrap();
    drop(store);

    let source = Connection::open_with_flags(
        root.join("workbench.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let backup = root.join("roundtrip-backup.sqlite3");
    source.backup("main", &backup, NO_PROGRESS).unwrap();
    let restored_dir = root.join("restored");
    fs::create_dir(&restored_dir).unwrap();
    let mut restored = Connection::open(restored_dir.join("workbench.sqlite3")).unwrap();
    restored.restore("main", &backup, NO_PROGRESS).unwrap();
    assert_eq!(
        restored
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    for table in [
        "jobs",
        "segments",
        "job_index",
        "restart_attempts",
        "restart_segments",
        "gpu_attempt_segments",
    ] {
        let sql = format!("SELECT count(*) FROM {table}");
        assert_eq!(
            source.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap(),
            restored
                .query_row(&sql, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            "{table}"
        );
    }
    drop(restored);
    drop(source);
    let restored = Store::open(&restored_dir).unwrap();
    assert_eq!(restored.get(new_id).unwrap(), expected_job);
    assert_eq!(
        restored.segments(new_id, 0, 200).unwrap(),
        expected_segments
    );
    assert_eq!(
        restored.get(id).unwrap()["unknown"],
        fixture["job"]["unknown"]
    );
    assert_eq!(fs::read(&audio_path).unwrap(), audio);
    fs::write(root.join("new-job.json"), expected_job.to_string()).unwrap();
    println!(
        "旧SQLite {} / 新SQLite {}: 正規WAL、履歴編集、新規字幕、backup/restoreを確認",
        fixture["sqlite"].as_str().unwrap(),
        rusqlite::version()
    );
}
