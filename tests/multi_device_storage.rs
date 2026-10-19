use migate::{
    device::{self, DeviceKey, FunctionDescription, FunctionKind, PhysicalDeviceDescription},
    storage::{self, CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{self, capability::parse_device_for_model, cloud::group_id},
};
use rusqlite::Connection;
#[path = "../src/storage/tests/fixtures.rs"]
mod publication_support;
use publication_support::{commit, record, scope};
fn description(id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        "米家插座",
        ["outlet_1", "outlet_2", "usb"]
            .into_iter()
            .map(|id| FunctionDescription {
                id: id.into(),
                label: id.into(),
                kind: FunctionKind::Outlet,
            })
            .collect(),
    )
    .unwrap()
}
fn cached(uid: &str, home: &str) -> CachedPhysicalDevice {
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: home.into(),
        group_id: group_id(uid, home),
        model: "cuco.plug.cp7pd".into(),
        instance_type: "urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:1".into(),
        device: parse_device_for_model(
            DeviceKey::new("xiaomi", "multi"),
            "cuco.plug.cp7pd",
            &serde_json::from_str(include_str!("fixtures/miot/cuco-cp7pd.json")).unwrap(),
        )
        .unwrap(),
    }
}

#[test]
fn stored_identity_names_and_endpoints_survive_withdrawal_and_republication() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = record(&description("multi"), 2);
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), vec![first.clone()]))
        .unwrap();
    store
        .devices()
        .set_device_label(&first.key, "Home name")
        .unwrap();
    let mut withdrawn = store
        .devices()
        .load_publication(None)
        .unwrap()
        .records
        .remove(0);
    withdrawn.published = false;
    let other = record(&description("other"), 6);
    store
        .devices()
        .apply_publication(&commit(
            scope(),
            Vec::new(),
            vec![withdrawn.clone(), other.clone()],
        ))
        .unwrap();
    assert!(
        store
            .devices()
            .load_published(&first.key)
            .unwrap()
            .is_none()
    );
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let mut restored = store
        .devices()
        .load_publication(None)
        .unwrap()
        .records
        .remove(0);
    assert_eq!(restored, withdrawn);
    restored.published = true;
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), vec![restored.clone(), other]))
        .unwrap();
    let saved = store.devices().load_published(&first.key).unwrap().unwrap();
    assert_eq!(saved.unique_id, first.unique_id);
    assert_eq!(saved.endpoint, 2);
    assert_eq!(
        saved
            .functions
            .iter()
            .map(|f| f.endpoint)
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(saved.label, "Home name");
    assert_eq!(store.devices().next_endpoint().unwrap(), 10);
    let mut incompatible = restored.clone();
    incompatible.functions.pop();
    assert!(
        store
            .devices()
            .apply_publication(&commit(scope(), Vec::new(), vec![incompatible]))
            .is_err()
    );
    assert_eq!(
        store.devices().load_published(&first.key).unwrap(),
        Some(restored)
    );
}

#[test]
fn names_are_utf8_truncated_on_import_and_validated_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = description("multi");
    let mut functions = original.functions().to_vec();
    functions[0].label = "路".repeat(20);
    let desc =
        PhysicalDeviceDescription::new(original.key().clone(), "插".repeat(20), functions).unwrap();
    let saved = record(&desc, 2);
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), vec![saved.clone()]))
        .unwrap();
    assert_eq!(saved.label, "插".repeat(10));
    assert_eq!(saved.functions[0].label, "路".repeat(10));
    assert!(
        store
            .devices()
            .set_device_label(desc.key(), &"x".repeat(33))
            .is_err()
    );
    assert_eq!(
        store.devices().load_published(desc.key()).unwrap(),
        Some(saved)
    );
}

#[test]
fn malformed_stored_functions_are_rejected_when_loading_a_publication() {
    for update in [
        "function_id=''",
        "label='abcdefghijklmnopqrstuvwxyz1234567'",
        "category='unknown'",
        "capabilities='unknown'",
        "endpoint=2",
        "endpoint=42",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let record = record(&description("multi"), 2);
        store
            .devices()
            .apply_publication(&commit(scope(), Vec::new(), vec![record.clone()]))
            .unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute(
                &format!("UPDATE device_function SET {update} WHERE function_id='usb'"),
                [],
            )
            .unwrap();
        assert!(store.devices().load_publication(None).is_err(), "{update}");
        assert!(
            store.devices().load_published(&record.key).is_err(),
            "{update}"
        );
    }
}

#[test]
fn invalid_records_and_function_write_failures_do_not_allocate_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = record(&description("multi"), 2);
    let mut empty = original.clone();
    empty.functions.clear();
    let mut duplicate = original.clone();
    duplicate.functions[1].key = duplicate.functions[0].key.clone();
    let mut missing_id = original.clone();
    missing_id.functions[0].key.id.clear();
    let mut wildcard = original.clone();
    wildcard.functions[2].endpoint = u16::MAX;
    for invalid in [empty, duplicate, missing_id, wildcard] {
        assert!(
            store
                .devices()
                .apply_publication(&commit(scope(), Vec::new(), vec![invalid]))
                .is_err()
        );
    }
    let db = Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_function AFTER INSERT ON device_function WHEN NEW.function_id='usb' BEGIN SELECT RAISE(ABORT,'secret'); END;").unwrap();
    assert!(
        store
            .devices()
            .apply_publication(&commit(scope(), Vec::new(), vec![original]))
            .is_err()
    );
    assert!(
        store
            .devices()
            .load_publication(None)
            .unwrap()
            .records
            .is_empty()
    );
    assert_eq!(store.devices().next_endpoint().unwrap(), 2);
    db.execute_batch("DROP TRIGGER fail_function;").unwrap();
    let original = description("last");
    let single = PhysicalDeviceDescription::new(
        original.key().clone(),
        original.label(),
        original.functions()[..1].to_vec(),
    )
    .unwrap();
    let last = record(&single, 65534);
    store
        .devices()
        .apply_publication(&commit(scope(), Vec::new(), vec![last.clone()]))
        .unwrap();
    assert_eq!(
        store.devices().load_published(&last.key).unwrap(),
        Some(last)
    );
    assert_eq!(store.devices().next_endpoint().unwrap(), 65535);
}

#[test]
fn cache_and_selected_scope_are_account_and_home_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = cached("account", "home-one");
    // A distinct but fully valid native description still keys the cache by
    // account/home scope instead of the device identity.
    let instance: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/miot/outlet.json")).unwrap();
    let second = CachedPhysicalDevice {
        uid: "account".into(),
        home_id: "home-two".into(),
        group_id: group_id("account", "home-two"),
        model: "cuco.plug.v3".into(),
        instance_type: instance["type"].as_str().unwrap().into(),
        device: parse_device_for_model(
            DeviceKey::new("xiaomi", "multi"),
            "cuco.plug.v3",
            &instance,
        )
        .unwrap(),
    };
    let other = cached("other", "home-one");
    let identity = record(&first.physical_description("Outlet"), 2);
    for cached in [&first, &second, &other] {
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
    assert_eq!(
        store
            .devices()
            .load_scope("account")
            .unwrap()
            .unwrap()
            .home_id,
        "home-two"
    );
    assert_eq!(
        store
            .devices()
            .load_scope("other")
            .unwrap()
            .unwrap()
            .home_id,
        "home-one"
    );
    for cached in [&first, &second, &other] {
        let scope = CachedScope {
            uid: cached.uid.clone(),
            home_id: cached.home_id.clone(),
            group_id: cached.group_id.clone(),
        };
        assert_eq!(
            store
                .devices()
                .load_publication(Some(&scope))
                .unwrap()
                .cached,
            std::slice::from_ref(cached)
        );
    }
    assert!(
        store
            .devices()
            .load_parsed("account", "missing", first.device.key())
            .unwrap()
            .is_none()
    );
}
