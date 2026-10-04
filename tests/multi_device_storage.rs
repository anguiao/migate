use migate::{
    device::{
        Capability, Category, DeviceKey, FunctionDescription, FunctionKey,
        PhysicalDeviceDescription,
    },
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::{ParsedDevice, ParsedFunction, PropertyBinding},
        cloud::group_id,
    },
};
use rusqlite::Connection;
fn description(id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: "米家插座".into(),
        functions: ["outlet_1", "outlet_2", "usb"]
            .into_iter()
            .map(|id| FunctionDescription {
                id: id.into(),
                label: id.into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            })
            .collect(),
    }
}
fn cached(uid: &str, home: &str) -> CachedPhysicalDevice {
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: home.into(),
        group_id: group_id(uid, home),
        model: "cuco.plug.cp7pd".into(),
        instance_type: "urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:1".into(),
        device: ParsedDevice {
            description: migate::device::DeviceDescription {
                key: DeviceKey::new("xiaomi", "multi"),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            },
            functions: [3, 4, 9]
                .into_iter()
                .zip(["outlet_1", "outlet_2", "usb"])
                .map(|(siid, id)| ParsedFunction {
                    id: id.into(),
                    label: id.into(),
                    power: PropertyBinding { siid, piid: 1 },
                })
                .collect(),
            refresh_trigger: Some(PropertyBinding { siid: 2, piid: 1 }),
        },
    }
}

#[test]
fn physical_and_function_identities_names_and_historical_endpoints_survive_remove_and_rejoin() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let desc = description("multi");
    let first = store.devices().publish(&desc).unwrap();
    assert_eq!(first.endpoint, 2);
    assert_eq!(
        first
            .functions
            .iter()
            .map(|f| f.endpoint)
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    let key = FunctionKey::new(desc.key.clone(), "usb");
    store
        .devices()
        .set_device_label(&desc.key, "Home name")
        .unwrap();
    store
        .devices()
        .set_function_label(&key, "USB name")
        .unwrap();
    store.devices().remove(&desc.key).unwrap();
    assert!(store.devices().list_published().unwrap().is_empty());
    let other = store.devices().publish(&description("other")).unwrap();
    assert_eq!(other.endpoint, 6);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let mut renamed = desc;
    renamed.label = "新米家名".into();
    let restored = store.devices().publish(&renamed).unwrap();
    assert_eq!(restored.unique_id, first.unique_id);
    assert_eq!(restored.endpoint, first.endpoint);
    assert_eq!(restored.label, "Home name");
    assert_eq!(restored.functions[2].label, "USB name");
    let mut incompatible = renamed;
    incompatible.functions.pop();
    assert!(store.devices().publish(&incompatible).is_err());
    assert_eq!(
        store.devices().load_published(&incompatible.key).unwrap(),
        Some(restored)
    );
}

#[test]
fn names_are_utf8_truncated_on_import_and_validated_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut desc = description("multi");
    desc.label = "插".repeat(20);
    desc.functions[0].label = "路".repeat(20);
    let saved = store.devices().publish(&desc).unwrap();
    assert_eq!(saved.label, "插".repeat(10));
    assert_eq!(saved.functions[0].label, "路".repeat(10));
    assert!(
        store
            .devices()
            .set_device_label(&desc.key, &"x".repeat(33))
            .is_err()
    );
    assert_eq!(
        store.devices().load_published(&desc.key).unwrap().unwrap(),
        saved
    );
}

#[test]
fn allocation_is_atomic_on_failure_and_exhaustion_and_never_uses_wildcard_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_function AFTER INSERT ON device_function WHEN NEW.function_id='usb' BEGIN SELECT RAISE(ABORT,'secret'); END;").unwrap();
    assert!(store.devices().publish(&description("failed")).is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM endpoint_allocation", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_function; INSERT INTO endpoint_allocation VALUES (65533,'test','history','main');").unwrap();
    let exhausted = store
        .devices()
        .publish(&description("exhausted"))
        .unwrap_err();
    assert!(matches!(
        exhausted,
        migate::storage::AdmissionError::EndpointExhausted
    ));
    assert_eq!(
        db.query_row("SELECT count(*) FROM device_identity", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    let mut single = description("last");
    single.functions.truncate(1);
    single.functions[0].id = "main".into();
    let saved = store.devices().publish(&single).unwrap();
    assert_eq!(saved.endpoint, 65534);
    assert_eq!(saved.functions[0].endpoint, 65534);
    assert!(store.devices().publish(&description("overflow")).is_err());
    assert_eq!(
        store.devices().load_published(&single.key).unwrap(),
        Some(saved)
    );
}

#[test]
fn canonical_cache_and_selected_scope_are_account_and_home_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account", "home-one");
    let mut second = cached("account", "home-two");
    second.device.functions[0].power.siid = 30;
    store.devices().save_parsed(&first).unwrap();
    store.devices().save_parsed(&second).unwrap();
    store
        .devices()
        .save_parsed(&cached("other", "home-one"))
        .unwrap();
    let scope = CachedScope {
        uid: "account".into(),
        home_id: "home-one".into(),
        group_id: group_id("account", "home-one"),
    };
    store.devices().save_scope(&scope).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.devices().load_scope("account").unwrap(), Some(scope));
    assert_eq!(store.devices().load_scope("other").unwrap(), None);
    assert_eq!(
        store
            .devices()
            .list_parsed("account", "home-one")
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&first)
    );
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home-two", &second.device.description.key)
            .unwrap(),
        Some(second)
    );
    assert!(
        store
            .devices()
            .load_parsed("account", "missing", &first.device.description.key)
            .unwrap()
            .is_none()
    );
    let mut invalid = first;
    invalid.group_id = "wrong".into();
    assert!(store.devices().save_parsed(&invalid).is_err());
}

#[test]
fn publishing_cached_device_is_one_transaction_and_incompatible_changes_keep_the_old_cache() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = cached("account", "home");
    let published = store
        .devices()
        .publish_cached(&original, "初始名称")
        .unwrap();
    let mut incompatible = original.clone();
    incompatible.device.functions.pop();
    assert!(
        store
            .devices()
            .publish_cached(&incompatible, "不同名称")
            .is_err()
    );
    assert_eq!(
        store
            .devices()
            .load_parsed("account", "home", &original.device.description.key)
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        store
            .devices()
            .load_published(&original.device.description.key)
            .unwrap(),
        Some(published)
    );
    Connection::open(store.path()).unwrap().execute_batch("CREATE TRIGGER reject_new_cache AFTER INSERT ON device_cache BEGIN SELECT RAISE(ABORT,'private'); END;").unwrap();
    let mut another = original;
    another.device.description.key.id = "new".into();
    assert!(store.devices().publish_cached(&another, "new").is_err());
    assert!(
        store
            .devices()
            .load_published(&another.device.description.key)
            .unwrap()
            .is_none()
    );
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM endpoint_allocation", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        4
    );
}

#[test]
fn invalid_function_sets_are_rejected_atomically_and_device_count_has_no_sample_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for ids in [vec![], vec!["main", "main"], vec![""]] {
        let mut desc = description("invalid");
        desc.functions = ids
            .into_iter()
            .map(|id| FunctionDescription {
                id: id.into(),
                label: id.into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            })
            .collect();
        assert!(store.devices().publish(&desc).is_err());
        assert!(store.devices().list_published().unwrap().is_empty());
    }
    let db = Connection::open(store.path()).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM endpoint_allocation", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    for id in 0..20 {
        store
            .devices()
            .publish(&description(&format!("same-model-{id}")))
            .unwrap();
    }
    assert_eq!(store.devices().list_published().unwrap().len(), 20);
    assert_eq!(
        db.query_row("SELECT count(*) FROM endpoint_allocation", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        80
    );
}
