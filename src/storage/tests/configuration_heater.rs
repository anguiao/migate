use super::configuration::{corrupt, load, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration, xiaomi::capability::tests::source::SourceFixture,
};
use serde_json::Value;

const MODEL: &str = "yeelink.bhf_light.v13";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

#[test]
fn v13_round_trips_light_and_heating_only_bindings() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(&store, &planned);
    drop(store);
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&store);
    assert_eq!(restored, planned);
    assert!(restored.temperature_bindings.is_empty());
    let entry = &restored.devices[0];
    assert_eq!(
        entry
            .functions
            .iter()
            .map(|function| function.id.as_str())
            .collect::<Vec<_>>(),
        ["light:2", "heater:3", "fan:3:2", "fan:3:4"]
    );
    assert_eq!(entry.functions.len(), 4);
    let endpoints = entry
        .functions
        .iter()
        .map(|function| function.endpoint)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(endpoints.len(), entry.functions.len());
    let native = entry.native.as_ref().unwrap().device();
    let heater = native
        .functions()
        .iter()
        .find(|function| function.id() == "heater:3")
        .unwrap()
        .heater()
        .unwrap();
    assert_eq!((heater.power().siid, heater.power().piid), (3, 3));
    assert_eq!(
        (
            heater.temperature().property().siid,
            heater.temperature().property().piid,
            heater.temperature().grid().min(),
            heater.temperature().grid().max(),
            heater.temperature().grid().step(),
        ),
        (3, 8, 25, 45, 1)
    );
    let stored: Value = serde_json::from_str(&raw(&store)).unwrap();
    assert!(
        stored["devices"][0]["native"]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|function| function["kind"] == "heater")
    );
}

#[test]
fn stored_heater_rejects_float_target_temperature_encoding() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(&store, &planned);
    let mut stored: Value = serde_json::from_str(&raw(&store)).unwrap();
    stored["devices"][0]["native"]["functions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|function| function["kind"] == "heater")
        .unwrap()["temperature"]["format"] = serde_json::json!("float");
    corrupt(&store, &stored);
    assert!(store.devices().load_configuration().is_err());
}
