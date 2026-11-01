use super::configuration::{corrupt, fixture, load, next, raw, save, scope, seed};
use crate::device::{DeviceKey, FunctionKey};
use serde_json::{Value, json};

fn key(did: &str, id: &str) -> FunctionKey {
    FunctionKey::new(DeviceKey::new("xiaomi", did), id)
}

#[test]
fn bindings_round_trip_validate_and_survive_withdrawal_of_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let ac = fixture(
        "ac",
        "lumi.acpartner.mcn04",
        include_str!("../../../tests/fixtures/miot/lumi-mcn04.json"),
    );
    let sensor = fixture(
        "sensor",
        "miaomiaoce.sensor_ht.t6",
        include_str!("../../../tests/fixtures/miot/miaomiaoce-t6.json"),
    );
    let mut initial = seed(&store, vec![ac.clone(), sensor.clone()]);
    initial.temperature_bindings.insert(
        key("ac", "air-conditioner:3"),
        key("sensor", "temperature:3:1001"),
    );
    save(&store, &initial);
    assert_eq!(load(&store), initial);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, value) in [
        (
            "/temperature_bindings/0/target/function_id",
            json!("missing"),
        ),
        ("/temperature_bindings/0/source/device_id", json!("missing")),
        (
            "/temperature_bindings/0/source/function_id",
            json!("humidity:3:1008"),
        ),
        ("/temperature_bindings/0/source/source", json!("")),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
    let mut duplicate = valid.clone();
    let binding = duplicate["temperature_bindings"][0].clone();
    duplicate["temperature_bindings"]
        .as_array_mut()
        .unwrap()
        .push(binding);
    corrupt(&store, &duplicate);
    assert!(store.devices().load_configuration().is_err());
    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .unwrap()
        .remove("temperature_bindings");
    corrupt(&store, &missing);
    assert!(store.devices().load_configuration().is_err());

    corrupt(&store, &valid);
    let withdrawn = next(&initial, scope("account", "home"), vec![ac.clone()]).configuration;
    assert_eq!(withdrawn.temperature_bindings, initial.temperature_bindings);
    save(&store, &withdrawn);
    assert_eq!(load(&store), withdrawn);
    let restored = next(&withdrawn, scope("account", "home"), vec![ac, sensor]).configuration;
    assert_eq!(restored.temperature_bindings, initial.temperature_bindings);
    assert_eq!(
        restored
            .device(&key("ac", "air-conditioner:3").device)
            .unwrap()
            .functions,
        initial
            .device(&key("ac", "air-conditioner:3").device)
            .unwrap()
            .functions
    );
    assert!(
        next(&restored, scope("another-account", "home"), vec![])
            .configuration
            .temperature_bindings
            .is_empty()
    );
    assert!(
        next(&restored, scope("account", "another-home"), vec![])
            .configuration
            .temperature_bindings
            .is_empty()
    );
}
