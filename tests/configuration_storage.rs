//! Public schema boundary: create the current format and reject old data unchanged.

use migate::storage::Store;
use rusqlite::Connection;
use std::{fs, path::Path};

fn snapshot(dir: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut entries = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn the_old_schema_is_rejected_without_modification() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("data");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("state.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "PRAGMA user_version = 8;
         CREATE TABLE marker (value TEXT);
         INSERT INTO marker VALUES ('old');",
    )
    .unwrap();
    drop(db);
    let before = snapshot(&dir);
    let error = Store::open(&dir)
        .err()
        .expect("the old schema must be rejected untouched");
    assert!(error.to_string().contains("validate database schema"));
    assert!(error.to_string().contains("new data directory"));
    assert_eq!(snapshot(&dir), before);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT value FROM marker", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "old"
    );
}

#[test]
fn schema9_is_one_private_singleton() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let db = Connection::open(store.path()).unwrap();
    let version = db
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(version, 9);
    let rows = db
        .query_row("SELECT count(*) FROM bridge_configuration", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(rows, 1);
    for table in [
        "device_cache",
        "device_identity",
        "device_function",
        "endpoint_allocation",
        "xiaomi_scope",
        "native_group",
        "native_group_member",
        "publication_root",
    ] {
        let exists = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .unwrap();
        assert!(!exists, "legacy table {table} must be removed");
    }
}
