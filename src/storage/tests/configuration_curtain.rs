use super::configuration::{corrupt, fixture, load, native, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, PhysicalDevice},
};

#[test]
fn curtain_configuration_reloads_with_stable_identity_and_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "curtain",
        "xiaomi.curtain.acn010",
        include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json"),
    );
    let mut planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    planned.devices[0].label = "Bedroom curtain".into();
    let entry = &planned.devices[0];
    assert_eq!(entry.functions.len(), 1);
    assert_eq!(entry.functions[0].id, "curtain:2");
    assert_ne!(entry.functions[0].endpoint, entry.endpoint);
    save(&store, &planned);
    drop(store);
    let reopened = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&reopened);
    assert_eq!(restored, planned);
    let entry = &restored.devices[0];
    assert_eq!(entry.label, "Bedroom curtain");
    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![entry.native.clone().unwrap()],
    );
    assert!(!unchanged.changed);
    assert!(!unchanged.rebuild);
    let key = DeviceKey::new("xiaomi", "curtain");
    let (device, _) = PhysicalDevice::new(restored.derive().served[&key].description.clone());
    assert_eq!(device.functions()[0].snapshot().power(), None);
}

#[test]
fn corrupted_curtain_bindings_cannot_be_restored() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "curtain",
        "xiaomi.curtain.acn010",
        include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let valid: serde_json::Value = serde_json::from_str(&raw(&store)).unwrap();
    for field in ["motor", "status", "current", "target"] {
        let mut invalid = valid.clone();
        invalid["devices"][0]["native"]["functions"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        corrupt(&store, &invalid);
        assert!(
            store.devices().load_configuration().is_err(),
            "missing {field}"
        );
    }
    for (pointer, value) in [
        (
            "/devices/0/native/functions/0/target/min",
            serde_json::json!(1),
        ),
        (
            "/devices/0/native/functions/0/status/closing",
            serde_json::json!(0),
        ),
        (
            "/devices/0/native/functions/0/motor/property/piid",
            serde_json::json!(0),
        ),
        (
            "/devices/0/native/functions/0/current/property/piid",
            serde_json::json!(4),
        ),
        (
            "/devices/0/native/functions/0/status/property/siid",
            serde_json::json!(40),
        ),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
    save(&store, &planned);
    assert_eq!(load(&store), planned);
}

#[test]
fn old_outlet_remains_readable_when_a_curtain_is_added() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let outlet = fixture(
        "outlet",
        "cuco.plug.v3",
        include_str!("../../../tests/fixtures/miot/outlet.json"),
    );
    let mut old = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![outlet.clone()],
    )
    .configuration;
    old.devices[0].label = "Desk".into();
    save(&store, &old);
    let old = load(&store);
    let curtain = fixture(
        "curtain",
        "xiaomi.curtain.acn010",
        include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json"),
    );
    let planned = next(&old, scope("account", "home"), vec![outlet, curtain]).configuration;
    save(&store, &planned);
    drop(store);
    let restored = load(&crate::storage::Store::open(directory.path()).unwrap());
    assert_eq!(restored, planned);
    let restored_outlet = restored
        .devices
        .iter()
        .find(|device| device.key.id == "outlet")
        .unwrap();
    assert_eq!(restored_outlet, &old.devices[0]);
    let restored_curtain = restored
        .devices
        .iter()
        .find(|device| device.key.id == "curtain")
        .unwrap();
    assert_ne!(restored_curtain.endpoint, restored_outlet.endpoint);
    assert_ne!(restored_curtain.unique_id, restored_outlet.unique_id);
}

#[test]
fn two_curtains_keep_independent_identities_names_and_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let text = include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json");
    let curtains = ["fabric", "sheer"]
        .into_iter()
        .map(|id| fixture(id, "xiaomi.curtain.acn010", text))
        .collect();
    let mut planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        curtains,
    )
    .configuration;
    planned.devices[0].label = "Fabric".into();
    planned.devices[1].label = "Sheer".into();
    save(&store, &planned);
    drop(store);
    let restored = load(&crate::storage::Store::open(directory.path()).unwrap());
    assert_eq!(restored, planned);
    let [first, second] = restored.devices.as_slice() else {
        panic!("expected two curtains")
    };
    assert_ne!(first.unique_id, second.unique_id);
    assert_ne!(first.endpoint, second.endpoint);
    assert_ne!(first.functions[0].endpoint, first.endpoint);
    assert_ne!(second.functions[0].endpoint, second.endpoint);
    assert_eq!((&*first.label, &*second.label), ("Fabric", "Sheer"));
}

#[test]
fn curtain_step_changes_replace_native_bindings_without_changing_publication_shape() {
    let text = include_str!("../../../tests/fixtures/miot/xiaomi-acn010.json");
    let original = fixture("curtain", "xiaomi.curtain.acn010", text);
    let current = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    let mut instance: serde_json::Value = serde_json::from_str(text).unwrap();
    instance["services"][1]["properties"][3]["value-range"] = serde_json::json!([0, 100, 5]);
    let updated = native("curtain", "xiaomi.curtain.acn010", instance);
    assert!(!crate::configuration::same_control_description(
        &original, &updated
    ));
    let plan = next(&current, scope("account", "home"), vec![updated]);
    assert!(plan.rebuild);
    assert_eq!(plan.configuration.root_version, current.root_version);
    assert_eq!(
        plan.configuration.devices[0].configuration_version,
        current.devices[0].configuration_version
    );
    assert_eq!(
        plan.configuration.devices[0].unique_id,
        current.devices[0].unique_id
    );
    assert_eq!(
        plan.configuration.devices[0].functions,
        current.devices[0].functions
    );
}
