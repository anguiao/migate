use super::configuration::{load, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::FunctionKind,
    xiaomi::capability::{
        ActionBinding, LegacyArgument, LegacyCommandBinding, SharedFanChannel,
        tests::source::SourceFixture,
    },
};
use serde_json::{Value, json};

const MODEL: &str = "yeelink.bhf_light.v13";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

#[test]
fn v13_round_trips_two_shared_fans_and_their_low_speed_operations() {
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
    for (id, channel, piid, aiid, mode) in [
        ("fan:3:2", SharedFanChannel::Blow, 2, 4, "coolwind"),
        ("fan:3:4", SharedFanChannel::Ventilation, 4, 5, "venting"),
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
        assert_eq!(
            speed.direct_command,
            Some(
                LegacyCommandBinding::new(
                    "set_bh_mode",
                    vec![
                        LegacyArgument::String(mode.into()),
                        LegacyArgument::Integer(1),
                    ],
                )
                .unwrap()
            )
        );
    }
    let heater = native
        .functions()
        .iter()
        .find(|function| function.id() == "heater:3")
        .unwrap()
        .heater()
        .unwrap();
    assert_eq!(
        heater.diagnostic_mode(),
        Some(crate::xiaomi::capability::PropertyBinding { siid: 3, piid: 1 })
    );

    assert_eq!(
        heater.power_refresh_trigger(),
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
            .all(|fan| fan["speed"]["shared"]["channel"].is_string())
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
fn corrupted_shared_fan_operation_is_rejected_on_restore() {
    use super::configuration::corrupt;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(&store, &planned);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    let mut invalid = valid.clone();
    let fan = invalid["devices"][0]["native"]["functions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|function| function["id"] == "fan:3:2")
        .unwrap();
    fan["speed"]["shared"] = json!({"channel": "blow"});
    corrupt(&store, &invalid);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn v13_without_the_private_service_restores_the_direct_low_speed_path() {
    let mut instance = v13();
    instance["services"]
        .as_array_mut()
        .unwrap()
        .retain(|service| service["iid"] != 4);
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, instance)],
    )
    .configuration;
    save(&store, &planned);
    drop(store);
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&store);
    assert_eq!(restored, planned);
    let native = restored.devices[0].native.as_ref().unwrap().device();
    for id in ["fan:3:2", "fan:3:4"] {
        let speed = native
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap()
            .fan()
            .unwrap()
            .shared_speed()
            .unwrap();
        assert!(speed.cloud_action.is_none(), "{id}");
        assert!(speed.direct_command.is_some(), "{id}");
    }
}

#[test]
fn corrupted_shared_fan_path_encodings_are_rejected_on_restore() {
    use super::configuration::corrupt;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(&store, &planned);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (name, mutate) in [
        (
            "channel",
            Box::new(|fan: &mut Value| fan["speed"]["shared"]["channel"] = json!("ventilation"))
                as Box<dyn Fn(&mut Value)>,
        ),
        (
            "method",
            Box::new(|fan: &mut Value| {
                fan["speed"]["shared"]["direct_command"]["method"] = json!("set_power")
            }),
        ),
        (
            "args",
            Box::new(|fan: &mut Value| {
                fan["speed"]["shared"]["direct_command"]["args"] =
                    json!([{"type": "integer", "value": 1}])
            }),
        ),
        (
            "action",
            Box::new(|fan: &mut Value| {
                fan["speed"]["shared"]["cloud_action"] = json!({"siid": 4, "aiid": 5})
            }),
        ),
        (
            "both paths absent",
            Box::new(|fan: &mut Value| {
                fan["speed"]["shared"]["direct_command"] = Value::Null;
                fan["speed"]["shared"]["cloud_action"] = Value::Null;
            }),
        ),
        (
            "unknown speed field",
            Box::new(|fan: &mut Value| fan["speed"]["unexpected"] = Value::Bool(true)),
        ),
    ] {
        let mut invalid = valid.clone();
        let fan = invalid["devices"][0]["native"]["functions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|function| function["id"] == "fan:3:2")
            .unwrap();
        mutate(fan);
        corrupt(&store, &invalid);
        assert!(
            store.devices().load_configuration().is_err(),
            "{name} corruption was accepted"
        );
    }
}

#[test]
fn v13_cloud_only_low_speed_path_restores_without_direct_command() {
    use super::configuration::corrupt;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![SourceFixture::new("bath", MODEL, v13())],
    )
    .configuration;
    save(&store, &planned);
    let mut valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    let fan = valid["devices"][0]["native"]["functions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|function| function["id"] == "fan:3:2")
        .unwrap();
    fan["speed"]["shared"]["direct_command"] = Value::Null;
    corrupt(&store, &valid);

    let restored = store
        .devices()
        .load_configuration()
        .expect("a valid cloud-only shared fan must restore");
    let native = restored.devices[0].native.as_ref().unwrap().device();
    let function = native
        .functions()
        .iter()
        .find(|function| function.id() == "fan:3:2")
        .unwrap();
    assert_eq!(
        function.kind(),
        FunctionKind::Fan {
            horizontal_swing: false,
            speed_count: 2,
        }
    );
    let speed = function.fan().unwrap().shared_speed().unwrap();
    assert_eq!(speed.channel, SharedFanChannel::Blow);
    assert_eq!(speed.cloud_action, Some(ActionBinding { siid: 4, aiid: 4 }));
    assert!(speed.direct_command.is_none());
}
