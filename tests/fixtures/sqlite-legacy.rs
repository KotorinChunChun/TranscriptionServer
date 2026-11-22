use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let folder = PathBuf::from(std::env::args().nth(1).expect("合成DBの保存先"));
    assert!(folder.is_absolute() && !folder.exists());
    fs::create_dir_all(&folder)?;
    let id = "0123456789abcdef0123456789abcdef";
    fs::create_dir(folder.join(id))?;
    fs::write(
        folder.join(id).join("source.media"),
        b"RIFF-synthetic-audio",
    )?;
    let db = Connection::open(folder.join("workbench.sqlite3"))?;
    db.execute_batch(
        "PRAGMA page_size=4096; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE jobs(id TEXT PRIMARY KEY,payload TEXT NOT NULL);
         CREATE TABLE segments(job_id TEXT NOT NULL,id INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(job_id,id));
         PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    let job = json!({"id":id,"createdAt":"2026-01-01T00:00:00.000Z",
        "updatedAt":"2026-01-01T00:00:01.000Z","revision":1,
        "fileName":"synthetic.wav","status":"complete","phase":"complete","engine":"whisper","modelId":"base",
        "clientId":"sqlite-compat-fixture","workload":"file","device":"cpu","task":"transcription",
        "segmentCount":1,"processedUntil":1.5,"rangeStart":0.0,"rangeEnd":1.5,
        "metadata":{"oldExtra":"保持","modelId":"base","language":"ja","resultGeneration":0},
        "unknown":{"nested":[1,"合成データ",null]}});
    let segment = json!({"id":1,"start":0.25,"end":1.5,"text":"旧版で保存した合成字幕","speaker":"話者1","oldExtra":"字幕の未知項目"});
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO jobs VALUES(?,?)", params![id, job.to_string()])?;
    tx.execute(
        "INSERT INTO segments VALUES(?,?,?)",
        params![id, 1, segment.to_string()],
    )?;
    tx.commit()?;
    let version: String = db.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    let version_number = rusqlite::version_number();
    assert!(version_number < 3_054_000);
    fs::write(
        folder.join("fixture-only.json"),
        json!({
            "purpose":"transcription-synthetic-sqlite","id":id,"sqlite":version,
            "sqliteNumber":version_number,"job":job,"segment":segment
        })
        .to_string(),
    )?;
    println!("旧SQLite {version} で合成DBと正規のWALを作成");
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    while !folder.join("release-old-connection").exists() {
        assert!(std::time::Instant::now() < deadline, "合成DB試験の待機上限");
        std::thread::sleep(Duration::from_millis(100));
    }
    let expected: Value = serde_json::from_slice(&fs::read(folder.join("new-job.json"))?)?;
    let new_id = expected["id"].as_str().unwrap();
    let text: String = db.query_row(
        "SELECT payload FROM segments WHERE job_id=? AND id=1",
        [id],
        |r| r.get(0),
    )?;
    let old_segment: Value = serde_json::from_str(&text)?;
    assert_eq!(old_segment["text"], "新版で編集した合成字幕");
    assert_eq!(old_segment["oldExtra"], segment["oldExtra"]);
    let raw: String = db.query_row("SELECT payload FROM jobs WHERE id=?", [new_id], |r| {
        r.get(0)
    })?;
    let actual: Value = serde_json::from_str(&raw)?;
    assert_eq!(actual["status"], "complete");
    assert_eq!(actual["segmentCount"], 2);
    assert_eq!(actual["metadata"]["compatibility"], "new-write");
    let check: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    assert_eq!(check, "ok");
    fs::write(
        folder.join("old-reopened.json"),
        json!({"sqlite":version,"check":check,"newJob":new_id,"oldReadNewWrites":true}).to_string(),
    )?;
    println!("旧SQLiteで新版の字幕・新規ジョブを再読込し、整合性を確認");
    Ok(())
}
