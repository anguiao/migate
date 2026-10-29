use migate::{config::Config, storage::Store};
use rusqlite::Connection;
use std::{error::Error as _, fs};

#[test]
fn initializes_and_restores_independent_identities() {
    let root = tempfile::tempdir().unwrap();
    for dir in [root.path().join("new"), root.path().join("empty")] {
        if dir.ends_with("empty") {
            fs::create_dir(&dir).unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            Connection::open(store.path())
                .unwrap()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            9
        );
        let identity = store.load_identity().unwrap();
        assert_eq!(identity.bridge_id.len(), 32);
        let db = Connection::open(store.path()).unwrap();
        let columns = db
            .prepare("PRAGMA table_info(identity)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(columns, ["id", "bridge_id"]);
        drop(store);
        assert_eq!(
            Store::open(&dir).unwrap().load_identity().unwrap(),
            identity
        );
    }
    assert_ne!(
        Store::open(root.path().join("new"))
            .unwrap()
            .load_identity()
            .unwrap(),
        Store::open(root.path().join("empty"))
            .unwrap()
            .load_identity()
            .unwrap()
    );
}

#[test]
fn raw_blobs_are_durable_on_each_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let matter = store.matter();
    let observer = Store::open(dir.path()).unwrap().matter();
    let blob = [0, 255, 42, 128];
    matter.put(7, &blob).unwrap();
    assert!(observer.contains(7).unwrap());
    assert_eq!(observer.get(7).unwrap().as_deref(), Some(blob.as_slice()));
    matter.put(7, b"updated").unwrap();
    assert_eq!(
        observer.get(7).unwrap().as_deref(),
        Some(b"updated".as_slice())
    );
    matter.put(u16::MAX, &[]).unwrap();
    assert!(observer.contains(u16::MAX).unwrap());
    assert_eq!(observer.get(u16::MAX).unwrap(), Some(vec![]));
    assert!(!matter.contains(8).unwrap());
    assert_eq!(matter.get(8).unwrap(), None);
    matter.delete(8).unwrap();
    matter.delete(7).unwrap();
    assert!(!observer.contains(7).unwrap());
    assert_eq!(observer.get(7).unwrap(), None);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.matter().get(7).unwrap(), None);
    assert_eq!(reopened.matter().get(u16::MAX).unwrap(), Some(vec![]));
    assert_eq!(
        reopened.load_identity().unwrap(),
        store.load_identity().unwrap()
    );
}

#[test]
fn matter_handles_share_data_and_outlive_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let other = store.clone();
    let first = store.matter();
    drop(store);
    let second = other.matter();
    drop(other);

    first.put(7, b"first").unwrap();
    assert_eq!(second.get(7).unwrap().as_deref(), Some(b"first".as_slice()));
    let clone = second.clone();
    drop(second);
    clone.put(7, b"updated").unwrap();
    assert_eq!(
        first.get(7).unwrap().as_deref(),
        Some(b"updated".as_slice())
    );
}

#[test]
fn corruption_and_partial_initialization_are_preserved() {
    for contents in [b"SECRET CONTENT".as_slice(), b""] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        fs::write(&path, contents).unwrap();
        let error = Store::open(dir.path())
            .and_then(|store| store.load_identity())
            .unwrap_err();
        assert!(error.to_string().contains(path.to_str().unwrap()));
        assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
        assert_eq!(fs::read(path).unwrap(), contents);
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("leftover"), b"secret").unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert!(!dir.path().join("state.db").exists());
    assert_eq!(fs::read(dir.path().join("leftover")).unwrap(), b"secret");
}

#[test]
fn missing_identity_is_not_regenerated() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path())
        .unwrap()
        .matter()
        .put(7, b"secret")
        .unwrap();
    let db = Connection::open(dir.path().join("state.db")).unwrap();
    db.execute("DELETE FROM identity", []).unwrap();
    assert!(Store::open(dir.path()).unwrap().load_identity().is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM identity", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT value FROM blobs WHERE key = 7", [], |row| row
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        b"secret"
    );
}

#[test]
fn failed_mutations_preserve_data_without_exposing_contents() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().matter();
    store.put(1, b"original").unwrap();
    let path = dir.path().join("state.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_update AFTER UPDATE ON blobs BEGIN
            SELECT RAISE(ABORT, 'SECRET CONTENT');
        END;
        CREATE TRIGGER reject_delete AFTER DELETE ON blobs BEGIN
            SELECT RAISE(ABORT, 'SECRET CONTENT');
        END;",
    )
    .unwrap();
    for (result, operation) in [
        (
            store.put(1, b"SECRET CONTENT"),
            "write Matter data for key 1",
        ),
        (store.delete(1), "delete Matter data for key 1"),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.path(), path);
        assert_eq!(error.operation(), operation);
        assert!(error.source().unwrap().is::<rusqlite::Error>());
        assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
        assert_eq!(
            store.get(1).unwrap().as_deref(),
            Some(b"original".as_slice())
        );
    }
    assert_eq!(
        Store::open(dir.path())
            .unwrap()
            .matter()
            .get(1)
            .unwrap()
            .as_deref(),
        Some(b"original".as_slice())
    );
}

#[test]
fn uses_final_config_directory_and_reports_io_failure() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::parse(
        ["--data-dir".into(), "selected".into()],
        Some("ignored".into()),
        None,
        Some("unused".into()),
        root.path(),
    )
    .unwrap();
    Store::open(&config.data_dir).unwrap();
    assert!(config.data_dir.join("state.db").is_file());
    assert!(!root.path().join("ignored").exists());
    let path = root.path().join("file");
    fs::write(&path, b"secret").unwrap();
    let error = Store::open(&path).err().unwrap();
    assert_eq!(error.path(), path);
    assert_eq!(error.operation(), "create data directory");
    assert!(error.source().unwrap().is::<std::io::Error>());
    assert_eq!(fs::read(path).unwrap(), b"secret");
}

#[cfg(unix)]
#[test]
fn private_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("private");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    Store::open(&dir)
        .unwrap()
        .matter()
        .put(1, b"secret")
        .unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("state.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn rejects_unsupported_or_damaged_schemas_without_changing_data() {
    for (setup, expected) in [
        (
            "PRAGMA user_version = 6; CREATE TABLE marker (value TEXT); INSERT INTO marker VALUES ('future');",
            "future",
        ),
        (
            "PRAGMA user_version = 1; CREATE TABLE marker (value TEXT); INSERT INTO marker VALUES ('missing');",
            "missing",
        ),
        (
            "CREATE TABLE identity (id INTEGER PRIMARY KEY, bridge_id INTEGER NOT NULL, light_id TEXT NOT NULL) STRICT;
             CREATE TABLE blobs (key INTEGER PRIMARY KEY, value BLOB NOT NULL) STRICT;
             CREATE TABLE marker (value TEXT); INSERT INTO marker VALUES ('damaged');",
            "damaged",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.join("state.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(setup).unwrap();
        drop(db);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let before = directory_snapshot(&dir);

        let error = Store::open(&dir).err().unwrap();
        assert_eq!(error.path(), path);
        // A rejected database must be left byte-for-byte and permission-for-
        // permission untouched: no journal, no chmod, no schema rewrite.
        assert_eq!(directory_snapshot(&dir), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        let db = Connection::open(error.path()).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM marker", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            expected
        );
    }
}

fn directory_snapshot(dir: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
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
fn invalid_current_schema_reports_sql_errors_without_replacing_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let db = Connection::open(store.path()).unwrap();
    drop(store);
    db.execute_batch("ALTER TABLE blobs DROP COLUMN value;")
        .unwrap();

    let reopened = Store::open(dir.path()).unwrap();
    assert!(reopened.matter().get(7).is_err());
    assert_eq!(reopened.load_identity().unwrap(), identity);
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        9
    );
}
