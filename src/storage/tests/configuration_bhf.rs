use super::configuration::{corrupt, load, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::FunctionKind,
    storage::Store,
    xiaomi::capability::{ActionBinding, SharedFanChannel, tests::source::SourceFixture},
};
use serde_json::{Value, json};

const MODEL: &str = "yeelink.bhf_light.v13";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn save_v13(store: &Store) -> CommittedConfiguration {
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(store, &planned);
    planned
}

fn shared_binding<'a>(configuration: &'a mut Value, id: &str) -> &'a mut Value {
    let function = configuration["devices"][0]["native"]["functions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|function| function["id"] == id)
        .unwrap();
    &mut function["speed"]["shared"]
}

#[test]
fn v13_round_trips_two_shared_fans_and_their_low_speed_operations() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let planned = save_v13(&store);
    drop(store);
    let store = Store::open(directory.path()).unwrap();
    let restored = load(&store);
    assert_eq!(restored, planned);

    let entry = &restored.devices[0];
    let endpoints = entry
        .functions
        .iter()
        .map(|function| (function.id.as_str(), function.endpoint))
        .collect::<Vec<_>>();
    for id in ["fan:3:2", "fan:3:4"] {
        assert!(
            endpoints.iter().any(|(actual, _)| *actual == id),
            "{endpoints:?}"
        );
    }
    let distinct = endpoints
        .iter()
        .map(|(_, endpoint)| *endpoint)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct.len(), endpoints.len());

    let native = entry.native.as_ref().unwrap().device();
    for (id, channel, piid, aiid) in [
        ("fan:3:2", SharedFanChannel::Blow, 2, 4),
        ("fan:3:4", SharedFanChannel::Ventilation, 4, 5),
    ] {
        let function = native
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap();
        assert_eq!(
            function.kind(),
            FunctionKind::Fan {
                horizontal_swing: false,
                speed_count: 2,
            }
        );
        assert_eq!(
            function.power(),
            Some(crate::xiaomi::capability::PropertyBinding { siid: 3, piid })
        );
        let speed = function.fan().unwrap().shared_speed().unwrap();
        assert_eq!(speed.channel, channel);
        assert_eq!(speed.cloud_action, Some(ActionBinding { siid: 4, aiid }));
    }
    let heater = native
        .functions()
        .iter()
        .find(|function| function.id() == "heater:3")
        .unwrap()
        .heater()
        .unwrap();
    assert_eq!(
        heater.diagnostic_mode().map(|mode| mode.property),
        Some(crate::xiaomi::capability::PropertyBinding { siid: 3, piid: 1 })
    );

    assert_eq!(
        heater
            .power_refresh_trigger()
            .map(|trigger| trigger.property),
        Some(crate::xiaomi::capability::PropertyBinding { siid: 3, piid: 5 })
    );

    let raw: Value = serde_json::from_str(&raw(&store)).unwrap();
    let fans = raw["devices"][0]["native"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|function| function["kind"] == "fan")
        .collect::<Vec<_>>();
    assert_eq!(fans.len(), 2);
    assert!(
        fans.iter()
            .all(|fan| fan["speed"]["shared"]["channel"].is_string()
                && fan["speed"]["shared"].get("direct_command").is_none())
    );

    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![SourceFixture::committed(&restored, "bath")],
    );
    assert!(!unchanged.changed && !unchanged.rebuild);
    assert_eq!(unchanged.configuration.devices, restored.devices);
}

#[test]
fn previous_v13_configuration_restores_the_same_identity_and_bindings() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let planned = save_v13(&store);
    let current: Value = serde_json::from_str(&raw(&store)).unwrap();
    let mut previous = current.clone();
    for (id, mode) in [("fan:3:2", "coolwind"), ("fan:3:4", "venting")] {
        shared_binding(&mut previous, id)["direct_command"] = json!({
            "method": "set_bh_mode",
            "args": [{"type": "string", "value": mode}, {"type": "integer", "value": 1}]
        });
    }
    corrupt(&store, &previous);
    let restored = load(&store);
    assert_eq!(restored, planned);
    save(&store, &restored);
    assert_eq!(
        serde_json::from_str::<Value>(&raw(&store)).unwrap(),
        current
    );
}

#[test]
fn restored_heater_feedback_uses_the_original_instance_enum_tables() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut instance = v13();
    let properties = instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 3)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap();
    for (piid, value) in [(1, 6), (5, 3)] {
        let property = properties
            .iter_mut()
            .find(|property| property["iid"] == piid)
            .unwrap();
        property["value-list"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value": value, "description": "Extension"}));
    }
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, instance)],
    )
    .configuration;
    save(&store, &planned);
    let wire: Value = serde_json::from_str(&raw(&store)).unwrap();
    let record = wire["devices"][0]["native"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|function| function["id"] == "heater:3")
        .unwrap();
    assert_eq!(record["diagnostic_mode"], json!({"siid":3,"piid":1}));
    assert_eq!(record["power_refresh_trigger"], json!({"siid":3,"piid":5}));
    let restored = load(&store);
    assert_eq!(restored, planned);
    let heater = restored.devices[0]
        .native
        .as_ref()
        .unwrap()
        .device()
        .functions()
        .iter()
        .find_map(|function| function.heater())
        .unwrap();
    for (binding, value) in [
        (heater.diagnostic_mode().unwrap(), 6),
        (heater.power_refresh_trigger().unwrap(), 3),
    ] {
        assert!(binding.accepts(&json!(value)));
        assert!(!binding.accepts(&json!(255)));
    }
}

#[test]
fn stored_shared_fan_must_match_its_channel_and_cloud_binding() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    save_v13(&store);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (field, value) in [
        ("channel", json!("ventilation")),
        ("cloud_action", json!({"siid": 4, "aiid": 5})),
        ("unexpected", json!(true)),
    ] {
        let mut invalid = valid.clone();
        shared_binding(&mut invalid, "fan:3:2")[field] = value;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{field}");
    }
}
