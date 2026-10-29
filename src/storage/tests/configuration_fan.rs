use super::configuration::{fixture, load, next, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, PhysicalDevice, Power},
};

#[test]
fn fan_configuration_restores_shape_identity_and_unknown_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "fan",
        "dmaker.fan.p5c",
        include_str!("../../../tests/fixtures/miot/dmaker-p5c.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    drop(store);
    let reopened = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&reopened);
    assert_eq!(restored, planned);
    let entry = &restored.devices[0];
    assert_eq!(entry.functions[0].endpoint, entry.endpoint);
    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![entry.native.clone().unwrap()],
    );
    assert!(!unchanged.changed && !unchanged.rebuild);
    let (physical, _) = PhysicalDevice::new(
        restored.derive().served[&DeviceKey::new("xiaomi", "fan")]
            .description
            .clone(),
    );
    let snapshot = physical.functions()[0].snapshot();
    assert_eq!(snapshot.power(), Some(Power::Unknown));
    assert_eq!(snapshot.fan().unwrap().speed.value, None);
}

#[test]
fn fan_without_swing_round_trips_alongside_existing_outlet() {
    use super::configuration::{native, raw};
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let outlet = fixture(
        "outlet",
        "cuco.plug.v3",
        include_str!("../../../tests/fixtures/miot/outlet.json"),
    );
    let original = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![outlet.clone()],
    )
    .configuration;
    save(&store, &original);
    let old = load(&store);
    let mut description: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/miot/dmaker-p5c.json")).unwrap();
    description["services"][1]["properties"][3]["access"] = serde_json::json!(["read"]);
    let fan = native("fan", "dmaker.fan.p5c", description);
    let updated = next(&old, scope("account", "home"), vec![outlet, fan]).configuration;
    save(&store, &updated);
    let restored = load(&store);
    assert_eq!(restored, updated);
    assert!(raw(&store).contains("\"kind\":\"fan\""));
    let old_outlet = old
        .devices
        .iter()
        .find(|device| device.key.id == "outlet")
        .unwrap();
    let new_outlet = restored
        .devices
        .iter()
        .find(|device| device.key.id == "outlet")
        .unwrap();
    assert_eq!(old_outlet.endpoint, new_outlet.endpoint);
    assert_eq!(old_outlet.unique_id, new_outlet.unique_id);
    let restored_fan = restored
        .devices
        .iter()
        .find(|device| device.key.id == "fan")
        .unwrap();
    assert_eq!(
        restored_fan.native.as_ref().unwrap().device().functions()[0]
            .fan()
            .unwrap()
            .swing(),
        None
    );
}

#[test]
fn fan_stored_bindings_are_validated() {
    use super::configuration::{corrupt, raw};
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let fan = fixture(
        "fan",
        "dmaker.fan.p5c",
        include_str!("../../../tests/fixtures/miot/dmaker-p5c.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![fan],
    )
    .configuration;
    save(&store, &planned);
    let valid: serde_json::Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, value) in [
        (
            "/devices/0/native/functions/0/mode/straight",
            serde_json::json!(3),
        ),
        (
            "/devices/0/native/functions/0/mode/values",
            serde_json::json!([0, 0]),
        ),
        (
            "/devices/0/native/functions/0/speed/format",
            serde_json::json!("float"),
        ),
        (
            "/devices/0/native/functions/0/speed/property/piid",
            serde_json::json!(0),
        ),
        ("/devices/0/native/model", serde_json::json!("cuco.plug.v3")),
        (
            "/devices/0/native/instance_type",
            serde_json::json!("urn:miot-spec-v2:device:fan:0000A005:other-vendor:1"),
        ),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
}
