use migate::{
    device::{self, Capability, Category, DeviceKey, FunctionDescription},
    storage::{self, CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        self,
        capability::{ParsedDevice, ParsedFunction, PropertyBinding},
        cloud::group_id,
    },
};
use rusqlite::Connection;
#[path = "../src/storage/tests/fixtures.rs"]
mod publication_support;
use publication_support::{commit, record, scope};

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
                lighting: None,
            }],
            refresh_trigger: None,
        },
    }
}

#[test]
fn caches_are_account_scoped_and_identity_survives_restart_and_routing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account-one");
    let second = cached("account-two");
    let identity = record(&first.physical_description("Outlet"), 2);
    for cached in [&first, &second] {
        let scope = CachedScope {
            uid: cached.uid.clone(),
            home_id: cached.home_id.clone(),
            group_id: cached.group_id.clone(),
        };
        store
            .devices()
            .apply_publication(&commit(scope, vec![cached.clone()], vec![identity.clone()]))
            .unwrap();
    }
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    for cached in [&first, &second] {
        assert_eq!(
            store
                .devices()
                .load_parsed(&cached.uid, "home", &identity.key)
                .unwrap()
                .as_ref(),
            Some(cached)
        );
    }
    assert_eq!(
        store.devices().load_published(&identity.key).unwrap(),
        Some(identity.clone())
    );
    let mut moved = second;
    moved.home_id = "different-home".into();
    moved.group_id = group_id(&moved.uid, &moved.home_id);
    let scope = CachedScope {
        uid: moved.uid.clone(),
        home_id: moved.home_id.clone(),
        group_id: moved.group_id.clone(),
    };
    store
        .devices()
        .apply_publication(&commit(scope, vec![moved.clone()], vec![identity.clone()]))
        .unwrap();
    assert_eq!(
        store.devices().load_published(&identity.key).unwrap(),
        Some(identity)
    );
    assert_eq!(
        store
            .devices()
            .load_parsed(&moved.uid, &moved.home_id, &moved.device.key)
            .unwrap(),
        Some(moved)
    );
    let db = Connection::open(store.path()).unwrap();
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
    let identity = record(&first.physical_description("Outlet"), 2);
    store
        .devices()
        .apply_publication(&commit(scope(), vec![first], vec![identity.clone()]))
        .unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute("UPDATE device_cache SET category='unknown'", [])
        .unwrap();
    assert!(store.devices().load_publication(Some(&scope())).is_err());
    assert_eq!(
        db.query_row("SELECT category FROM device_cache", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "unknown"
    );
    db.execute("UPDATE device_identity SET unique_id='broken'", [])
        .unwrap();
    assert!(store.devices().load_published(&identity.key).is_err());
    assert_eq!(
        db.query_row("SELECT unique_id FROM device_identity", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "broken"
    );
}

#[test]
fn invalid_cache_or_late_write_failure_preserves_the_previous_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account");
    let identity = record(&first.physical_description("Outlet"), 2);
    let original = commit(scope(), vec![first.clone()], vec![identity.clone()]);
    store.devices().apply_publication(&original).unwrap();
    for invalid in [
        CachedPhysicalDevice {
            group_id: "wrong-family".into(),
            ..first.clone()
        },
        CachedPhysicalDevice {
            instance_type: "urn:miot-spec-v2:device:light:0000A001:vendor-v1:1".into(),
            ..first.clone()
        },
    ] {
        assert!(
            store
                .devices()
                .apply_publication(&commit(scope(), vec![invalid], vec![identity.clone()]))
                .is_err()
        );
    }
    Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER reject_cache AFTER INSERT ON device_cache BEGIN SELECT RAISE(ABORT, 'private details'); END;").unwrap();
    let mut changed = original;
    changed.cached[0].model = "changed.model".into();
    changed.records[0].label = "Must roll back".into();
    let error = store.devices().apply_publication(&changed).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private details"));
    let state = store.devices().load_publication(Some(&scope())).unwrap();
    assert_eq!(state.cached, [first]);
    assert_eq!(state.records, [identity]);
}
