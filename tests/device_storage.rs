use migate::{
    device::{Capability, Category, DeviceKey, FunctionDescription},
    storage::{CachedPhysicalDevice, Store},
    xiaomi::{
        capability::{ParsedDevice, ParsedFunction, PropertyBinding},
        cloud::group_id,
    },
};
use rusqlite::Connection;

fn cached(uid: &str) -> CachedPhysicalDevice {
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "home".into(),
        group_id: group_id(uid, "home"),
        model: "vendor.plug".into(),
        instance_type: "urn:miot-spec-v2:device:outlet:0000A002:vendor-v1:1".into(),
        device: ParsedDevice {
            key: DeviceKey::new("xiaomi", "did"),
            functions: vec![ParsedFunction {
                description: FunctionDescription {
                    id: "main".into(),
                    label: "主电源".into(),
                    category: Category::Outlet,
                    capabilities: vec![Capability::OnOff],
                },
                power: PropertyBinding { siid: 12, piid: 9 },
            }],
            refresh_trigger: None,
        },
    }
}

#[test]
fn caches_are_account_scoped_and_identity_is_stable_across_restart_and_routing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let first = cached("account-one");
    let second = cached("account-two");
    let key = &first.device.key;
    let store = Store::open(dir.path()).unwrap();
    let identity = store.devices().publish_cached(&first, "Outlet").unwrap();
    assert_eq!(identity.endpoint, 2);
    assert_eq!(identity.unique_id.len(), 32);
    assert_ne!(identity.unique_id, store.load_identity().unwrap().bridge_id);
    assert_eq!(
        store
            .devices()
            .load_parsed("account-one", "home", key)
            .unwrap(),
        Some(first.clone())
    );
    assert!(
        store
            .devices()
            .load_parsed("account-two", "home", key)
            .unwrap()
            .is_none()
    );
    store.devices().publish_cached(&second, "Outlet").unwrap();
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .devices()
            .load_parsed("account-one", "home", key)
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        reopened
            .devices()
            .load_parsed("account-two", "home", key)
            .unwrap(),
        Some(second.clone())
    );
    assert_eq!(
        reopened.devices().load_published(key).unwrap(),
        Some(identity.clone())
    );
    let mut moved = second;
    moved.home_id = "different-home".into();
    moved.group_id = group_id(&moved.uid, &moved.home_id);
    assert_eq!(
        reopened.devices().publish_cached(&moved, "Moved").unwrap(),
        identity
    );
    let mut another = moved.clone();
    another.device.key.id = "other-did".into();
    assert_ne!(
        reopened
            .devices()
            .publish_cached(&another, "Other outlet")
            .unwrap()
            .unique_id,
        identity.unique_id
    );
    let db = Connection::open(reopened.path()).unwrap();
    let columns = db
        .prepare("PRAGMA table_info(device_cache)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for dynamic in ["power", "online", "authenticated", "pending"] {
        assert!(!columns.iter().any(|column| column == dynamic));
    }
}

#[test]
fn damaged_cache_and_identity_fail_without_replacing_stored_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account");
    let key = &first.device.key;
    store.devices().publish_cached(&first, "Outlet").unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute("UPDATE device_cache SET category = 'unknown'", [])
        .unwrap();
    assert!(store.devices().load_parsed("account", "home", key).is_err());
    assert_eq!(
        db.query_row("SELECT category FROM device_cache", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "unknown"
    );
    db.execute("UPDATE device_identity SET unique_id = 'broken'", [])
        .unwrap();
    assert!(store.devices().load_published(key).is_err());
    assert_eq!(
        db.query_row("SELECT unique_id FROM device_identity", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "broken"
    );
    let mut invalid = first.clone();
    invalid.group_id = "wrong-family".into();
    assert!(store.devices().publish_cached(&invalid, "Outlet").is_err());
    assert_eq!(
        db.query_row("SELECT group_id FROM device_cache", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        first.group_id
    );
}

#[test]
fn cache_write_failures_preserve_the_previous_description() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account");
    store.devices().publish_cached(&first, "Outlet").unwrap();
    Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER reject_cache AFTER UPDATE ON device_cache BEGIN SELECT RAISE(ABORT, 'private details'); END;").unwrap();
    let mut changed = first.clone();
    changed.model = "changed.model".into();
    let error = store
        .devices()
        .publish_cached(&changed, "Outlet")
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private details"));
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home", &first.device.key)
            .unwrap(),
        Some(first)
    );
}

#[test]
fn corrupted_cached_instance_type_cannot_restore_an_outlet_or_overwrite_valid_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account");
    store.devices().publish_cached(&first, "Outlet").unwrap();
    let mut invalid = first.clone();
    invalid.instance_type = "urn:miot-spec-v2:device:light:0000A001:vendor-v1:1".into();
    assert!(store.devices().publish_cached(&invalid, "Outlet").is_err());
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home", &first.device.key)
            .unwrap(),
        Some(first.clone())
    );
    let db = Connection::open(store.path()).unwrap();
    db.execute(
        "UPDATE device_cache SET instance_type = ?1",
        [&invalid.instance_type],
    )
    .unwrap();
    assert!(
        store
            .devices()
            .load_parsed("account", "home", &first.device.key)
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT instance_type FROM device_cache", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        invalid.instance_type
    );
}

#[test]
fn version_one_storage_upgrades_without_changing_pairing_or_bridge_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/storage-v2.sql"))
        .unwrap();
    db.execute_batch(
        "DROP TABLE device_cache; DROP TABLE device_identity; PRAGMA user_version = 1;",
    )
    .unwrap();
    db.execute(
        "INSERT INTO identity VALUES (1, ?1, ?2)",
        [
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ],
    )
    .unwrap();
    db.execute("INSERT INTO blobs VALUES (7, ?1)", [b"paired".as_slice()])
        .unwrap();
    drop(db);
    let upgraded = Store::open(dir.path()).unwrap();
    assert_eq!(
        upgraded.load_identity().unwrap().bridge_id,
        "11111111111111111111111111111111"
    );
    assert_eq!(upgraded.matter().get(7).unwrap(), Some(b"paired".to_vec()));
    assert!(upgraded.xiaomi().load().unwrap().is_none());
    upgraded
        .devices()
        .publish_cached(&cached("account"), "Outlet")
        .unwrap();
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );
}

#[test]
fn failed_version_one_upgrade_keeps_the_previous_version_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/storage-v2.sql"))
        .unwrap();
    db.execute_batch("DROP TABLE device_cache; DROP TABLE device_identity; PRAGMA user_version = 1; CREATE VIEW device_identity AS SELECT 'block migration' AS source;").unwrap();
    db.execute(
        "INSERT INTO identity VALUES (1, ?1, ?2)",
        [
            "11111111111111111111111111111111",
            "22222222222222222222222222222222",
        ],
    )
    .unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT bridge_id FROM identity", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "11111111111111111111111111111111"
    );
    assert!(
        !db.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'device_cache')",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
}
