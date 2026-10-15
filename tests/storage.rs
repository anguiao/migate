use migate::{
    config::Config,
    device::{
        Capability, Category, DeviceKey, FunctionDescription, FunctionKey, PhysicalDevice,
        PhysicalDeviceDescription, Power,
    },
    storage::{
        CachedPhysicalDevice, CachedScope, PublicationCommit, Store, StoredDevice, StoredFunction,
        StoredRelation, StoredRevocation,
    },
    xiaomi::{capability::parse_device_for_model, cloud::group_id},
};
use rusqlite::Connection;
use std::{error::Error as _, fs};

#[test]
fn initializes_and_restores_identity_without_power() {
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
            7
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
        let description = PhysicalDeviceDescription {
            key: DeviceKey::new("xiaomi", "test-outlet"),
            label: "Test Outlet".into(),
            functions: vec![FunctionDescription {
                id: "main".into(),
                label: "Main power".into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            }],
        };
        let (device, _receiver) = PhysicalDevice::new(description.clone());
        device.functions()[0].observe(true);
        drop(store);
        assert_eq!(
            Store::open(&dir).unwrap().load_identity().unwrap(),
            identity
        );
        assert_eq!(
            PhysicalDevice::new(description).0.functions()[0]
                .snapshot()
                .power,
            Power::Unknown
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

fn outlet_description(id: &str, label: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: label.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            category: Category::Outlet,
            capabilities: vec![Capability::OnOff],
        }],
    }
}

fn publication_scope() -> CachedScope {
    CachedScope {
        uid: "u1".into(),
        home_id: "h1".into(),
        group_id: group_id("u1", "h1"),
    }
}

fn light_cached(id: &str, scope: &CachedScope) -> CachedPhysicalDevice {
    let instance: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/miot/xiaomi-light-ceil04.json")).unwrap();
    let instance_type = instance["type"].as_str().unwrap().to_owned();
    let device = parse_device_for_model(
        DeviceKey::new("xiaomi", id),
        "xiaomi.light.ceil04",
        &instance,
    )
    .unwrap();
    CachedPhysicalDevice {
        uid: scope.uid.clone(),
        home_id: scope.home_id.clone(),
        group_id: scope.group_id.clone(),
        model: "xiaomi.light.ceil04".into(),
        instance_type,
        device,
    }
}

fn light_record(cached: &CachedPhysicalDevice) -> StoredDevice {
    let key = cached.device.key.clone();
    StoredDevice {
        key: key.clone(),
        unique_id: "0f0e0d0c0b0a09080706050403020100".into(),
        endpoint: 2,
        label: "New Light".into(),
        functions: vec![StoredFunction {
            key: FunctionKey::new(key, "main"),
            endpoint: 2,
            label: "主灯".into(),
            category: Category::Light,
            capabilities: cached.device.functions[0].description.capabilities.clone(),
        }],
        published: true,
        configuration_version: 1,
    }
}

#[test]
fn first_full_publication_inserts_real_cache_identity_and_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let scope = publication_scope();
    let cached = light_cached("new-light", &scope);
    let record = light_record(&cached);
    let relation = StoredRelation {
        group_did: "group-new".into(),
        record_did: "record-new".into(),
        model: "xiaomi.group".into(),
        instance_type: None,
        is_physical: true,
        active: false,
        error: Some("native group description has no instance type".into()),
        members: vec!["new-light".into()],
    };
    store
        .devices()
        .apply_publication(&PublicationCommit {
            scope: scope.clone(),
            cached: vec![cached.clone()],
            records: vec![record.clone()],
            relations: vec![relation.clone()],
            root_version: 2,
            clear_revocations: Vec::new(),
        })
        .unwrap();
    assert_eq!(store.devices().next_endpoint().unwrap(), 3);

    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    let state = reopened.devices().load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 2);
    assert_eq!(state.records, std::slice::from_ref(&record));
    assert_eq!(state.cached, std::slice::from_ref(&cached));
    assert_eq!(state.relations, [relation]);
    assert_eq!(
        reopened
            .devices()
            .load_parsed(&scope.uid, &scope.home_id, &record.key)
            .unwrap(),
        Some(cached)
    );
    let db = Connection::open(reopened.path()).unwrap();
    let allocations = db
        .prepare("SELECT endpoint,source,device_id,slot FROM endpoint_allocation")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, u16>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        allocations,
        [(2, "xiaomi".into(), "new-light".into(), "main".into())]
    );
}

#[test]
fn late_transaction_failure_rolls_back_the_whole_first_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let devices = store.devices();
    let scope = publication_scope();
    let cached = light_cached("rollback-light", &scope);
    let record = light_record(&cached);
    let relation = StoredRelation {
        group_did: "group-rollback".into(),
        record_did: "record-rollback".into(),
        model: "xiaomi.group".into(),
        instance_type: Some("urn:miot-spec-v2:device:light:0000A001:test-vendor:1".into()),
        is_physical: false,
        active: true,
        error: None,
        members: vec!["rollback-light".into()],
    };
    // Fail the last relation-member write so identity, cache, allocation and
    // root writes have already been staged inside the same transaction.
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER injected_failure BEFORE INSERT ON native_group_member
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
    )
    .unwrap();
    drop(db);

    let error = devices
        .apply_publication(&PublicationCommit {
            scope: scope.clone(),
            cached: vec![cached.clone()],
            records: vec![record],
            relations: vec![relation],
            root_version: 2,
            clear_revocations: Vec::new(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("commit published configuration"));
    // The injected trigger aborts the late relation-member write; the storage
    // error deliberately strips SQLite message text, so the extended code
    // proves the failure reached the late trigger, not earlier validation.
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<rusqlite::Error>()
        .unwrap();
    let rusqlite::Error::SqliteFailure(code, _) = source else {
        panic!("expected a SQLite failure, got {source:?}");
    };
    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);

    let state = devices.load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 1);
    assert!(state.records.is_empty());
    assert!(state.cached.is_empty());
    assert!(state.relations.is_empty());
    assert_eq!(devices.next_endpoint().unwrap(), 2);
    assert_eq!(
        devices
            .load_parsed(&scope.uid, &scope.home_id, &cached.device.key)
            .unwrap(),
        None
    );
}

#[test]
fn publication_commit_is_atomic_and_retains_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let devices = store.devices();
    let first = devices
        .publish(&outlet_description("first", "First"))
        .unwrap();
    let second = devices
        .publish(&outlet_description("second", "Second"))
        .unwrap();
    let scope = publication_scope();
    assert_eq!(devices.next_endpoint().unwrap(), 4);

    let mut committed_first = first.clone();
    committed_first.label = "First renamed".into();
    let unavailable = StoredRevocation {
        key: second.key.clone(),
        reason: "deleted from the account".into(),
    };
    devices
        .record_revocations(std::slice::from_ref(&unavailable))
        .unwrap();
    let mut withdrawn = second.clone();
    withdrawn.published = false;
    devices
        .apply_publication(&PublicationCommit {
            scope: scope.clone(),
            cached: Vec::new(),
            records: vec![committed_first.clone(), withdrawn.clone()],
            relations: vec![
                StoredRelation {
                    group_did: "group-1".into(),
                    record_did: "record-1".into(),
                    model: "xiaomi.group".into(),
                    instance_type: Some("urn:miot-spec-v2:device:light:group".into()),
                    is_physical: false,
                    active: true,
                    error: None,
                    members: vec!["first".into(), "second".into()],
                },
                StoredRelation {
                    group_did: "group-2".into(),
                    record_did: "record-2".into(),
                    model: "xiaomi.group".into(),
                    instance_type: None,
                    is_physical: true,
                    active: false,
                    error: Some("native group description has no instance type".into()),
                    members: Vec::new(),
                },
            ],
            root_version: 2,
            clear_revocations: Vec::new(),
        })
        .unwrap();

    let state = devices.load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 2);
    assert_eq!(state.revocations, std::slice::from_ref(&unavailable));
    assert_eq!(state.relations.len(), 2);
    assert_eq!(state.relations[0].members, ["first", "second"]);
    assert_eq!(state.relations[1].group_did, "group-2");
    assert!(!state.relations[1].active);
    assert_eq!(state.relations[1].instance_type, None);
    assert_eq!(
        state.relations[1].error.as_deref(),
        Some("native group description has no instance type")
    );
    assert!(state.relations[1].members.is_empty());
    let reloaded = |key: &DeviceKey| {
        state
            .records
            .iter()
            .find(|record| &record.key == key)
            .cloned()
            .unwrap()
    };
    assert_eq!(reloaded(&first.key).label, "First renamed");
    let retained = reloaded(&second.key);
    assert!(!retained.published);
    assert_eq!(retained.unique_id, second.unique_id);
    assert_eq!(retained.endpoint, second.endpoint);
    assert_eq!(retained.functions, second.functions);
    assert_eq!(retained.configuration_version, second.configuration_version);

    // A failing commit after earlier updates rolls the whole picture back.
    let mut invalid = withdrawn.clone();
    invalid.endpoint = 99;
    let mut early = committed_first;
    early.label = "Must not persist".into();
    let error = devices
        .apply_publication(&PublicationCommit {
            scope: scope.clone(),
            cached: Vec::new(),
            records: vec![early, invalid],
            relations: Vec::new(),
            root_version: 3,
            clear_revocations: Vec::new(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("commit published configuration"));
    let state = devices.load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 2);
    assert_eq!(state.relations.len(), 2);
    let after = |key: &DeviceKey| {
        state
            .records
            .iter()
            .find(|record| &record.key == key)
            .cloned()
            .unwrap()
    };
    assert_eq!(after(&first.key).label, "First renamed");
    assert!(!after(&second.key).published);
    assert_eq!(after(&second.key).endpoint, second.endpoint);
    assert_eq!(state.revocations, [unavailable]);

    // A newer confirmed authorized re-admission clears the tombstone.
    let mut re_admitted = withdrawn.clone();
    re_admitted.published = true;
    re_admitted.configuration_version = 2;
    let mut renamed = first.clone();
    renamed.label = "First renamed".into();
    devices
        .apply_publication(&PublicationCommit {
            scope: scope.clone(),
            cached: Vec::new(),
            records: vec![renamed, re_admitted],
            relations: Vec::new(),
            root_version: 3,
            clear_revocations: vec![second.key.clone()],
        })
        .unwrap();
    let state = devices.load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 3);
    assert!(state.revocations.is_empty());
    let second = state
        .records
        .iter()
        .find(|record| record.key.id == "second")
        .unwrap();
    assert!(second.published);
    assert_eq!(second.configuration_version, 2);

    // The root version never decreases, and a missing device stays retained.
    let mut lower = first.clone();
    lower.label = "Lower".into();
    assert!(
        devices
            .apply_publication(&PublicationCommit {
                scope: scope.clone(),
                cached: Vec::new(),
                records: vec![lower],
                relations: Vec::new(),
                root_version: 1,
                clear_revocations: Vec::new(),
            })
            .is_err()
    );
    let reopened = Store::open(dir.path()).unwrap();
    let state = reopened.devices().load_publication(Some(&scope)).unwrap();
    assert_eq!(state.root_version, 3);
    assert_eq!(state.records.len(), 2);
    let first = state
        .records
        .iter()
        .find(|record| record.key.id == "first")
        .unwrap();
    let second = state
        .records
        .iter()
        .find(|record| record.key.id == "second")
        .unwrap();
    assert_eq!(first.label, "First renamed");
    assert!(second.published);
    assert_eq!(second.unique_id, retained.unique_id);
    assert_eq!(first.functions.len(), 1);
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
        7
    );
}
