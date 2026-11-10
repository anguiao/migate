use super::*;
use crate::device::{DeviceKey, FunctionKind};
use serde_json::Value;

const MODEL: &str = "yeelink.bhf_light.v13";

fn v13() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/yeelink-bhf-light-v13.json"
    ))
    .unwrap()
}

fn parse(instance: &Value) -> Result<crate::xiaomi::capability::ParsedDevice, CapabilityError> {
    crate::xiaomi::capability::parse_device_for_model(
        DeviceKey::new("xiaomi", "bath"),
        MODEL,
        instance,
    )
}

#[test]
fn actual_v13_description_yields_light_heater_and_two_shared_fans() {
    let parsed = parse(&v13()).unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|function| (function.id(), function.label()))
            .collect::<Vec<_>>(),
        [
            ("light:2", "Light"),
            ("heater:3", "暖风"),
            ("fan:3:2", "吹风"),
            ("fan:3:4", "换气"),
        ]
    );

    for (id, power, channel_label) in [
        ("fan:3:2", PropertyBinding { siid: 3, piid: 2 }, "吹风"),
        ("fan:3:4", PropertyBinding { siid: 3, piid: 4 }, "换气"),
    ] {
        let function = parsed
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap_or_else(|| panic!("{channel_label} function is missing"));
        assert_eq!(
            function.kind(),
            FunctionKind::Fan {
                horizontal_swing: false,
                speed_count: 2,
            },
            "{id}"
        );
        assert_eq!(function.power(), Some(power), "{id}");
        assert_eq!(function.bindings(), [power], "{id}");
        assert!(function.fan().is_some(), "{id}");
    }

    let heater = parsed
        .functions()
        .iter()
        .find(|function| function.id() == "heater:3")
        .unwrap();
    assert!(
        heater
            .bindings()
            .contains(&PropertyBinding { siid: 3, piid: 1 }),
        "standard 3.1 mode must remain a separate diagnostic input"
    );
    assert!(
        !heater
            .bindings()
            .contains(&PropertyBinding { siid: 4, piid: 4 }),
        "a private action must never be disguised as a property"
    );
}

#[test]
fn v13_without_low_actions_still_keeps_both_fans_and_the_heater() {
    let mut instance = v13();
    instance["services"]
        .as_array_mut()
        .unwrap()
        .retain(|service| service["iid"] != 4);

    let parsed = parse(&instance).unwrap();
    let ids = parsed
        .functions()
        .iter()
        .map(|function| function.id())
        .collect::<Vec<_>>();
    for id in ["light:2", "heater:3", "fan:3:2", "fan:3:4"] {
        assert!(ids.contains(&id), "missing {id}: {ids:?}");
    }
}

#[test]
fn an_invalid_cloud_low_action_only_disables_that_path() {
    let mut instance = v13();
    let action = instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 4)
        .unwrap()["actions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|action| action["iid"] == 4)
        .unwrap();
    action["in"] = json!([{"piid": 1, "value": 1}]);

    let parsed = parse(&instance).unwrap();
    let blow = parsed
        .functions()
        .iter()
        .find(|function| function.id() == "fan:3:2")
        .unwrap()
        .fan()
        .unwrap()
        .shared_speed()
        .unwrap();
    assert!(blow.cloud_action.is_none());
    assert_eq!(blow.direct_command.as_ref().unwrap().method, "set_bh_mode");
    let diagnostics = parsed.recognition().unwrap().diagnostics();
    assert!(
        diagnostics.iter().any(|issue| {
            issue.target() == "fan:3:2" && issue.reason().contains("low-speed action")
        }),
        "{diagnostics:?}"
    );
}

#[test]
fn heater_built_in_temperature_does_not_depend_on_either_fan_being_admitted() {
    let mut instance = v13();
    let service = instance["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 3)
        .unwrap();
    service["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| property["iid"] != 2 && property["iid"] != 4);

    let parsed = parse(&instance).unwrap();
    assert!(
        parsed
            .functions()
            .iter()
            .any(|function| function.id() == "heater:3")
    );
    assert!(
        !parsed
            .functions()
            .iter()
            .any(|function| function.id().starts_with("fan:"))
    );
    let native = crate::xiaomi::capability::NativeDescription::new(
        MODEL.into(),
        instance["type"].as_str().unwrap().into(),
        parsed,
    )
    .unwrap();
    let shared = native
        .plan()
        .shared_feedback()
        .expect("v13 shared feedback is declared for the heater");
    assert_eq!(shared.heater_temperature.as_deref(), Some("heater:3"));
    assert!(shared.fans.is_empty());
}

#[test]
fn a_generic_ptc_heater_does_not_inherit_v13_related_rules() {
    let instance = json!({
        "type": "urn:miot-spec-v2:device:bath-heater:0000A028:example-vendor:1",
        "services": [{
            "iid": 3,
            "type": "urn:miot-spec-v2:service:ptc-bath-heater:0000783B:example-vendor:1",
            "properties": [
                {
                    "iid": 3,
                    "type": "urn:miot-spec-v2:property:heating:000000C7:example-vendor:1",
                    "format": "bool",
                    "access": ["read", "write", "notify"]
                },
                {
                    "iid": 8,
                    "type": "urn:miot-spec-v2:property:target-temperature:00000021:example-vendor:1",
                    "format": "uint8",
                    "unit": "celsius",
                    "access": ["read", "write", "notify"],
                    "value-range": [25, 45, 1]
                }
            ]
        }]
    });
    let parsed = crate::xiaomi::capability::parse_device_for_model(
        DeviceKey::new("xiaomi", "generic-bath"),
        "example.bath",
        &instance,
    )
    .unwrap();
    assert_eq!(
        parsed
            .functions()
            .iter()
            .map(|function| function.id())
            .collect::<Vec<_>>(),
        ["heater:3"]
    );
    let native = crate::xiaomi::capability::NativeDescription::new(
        "example.bath".into(),
        instance["type"].as_str().unwrap().into(),
        parsed,
    )
    .unwrap();
    assert!(
        native.plan().related().is_none(),
        "generic PTC heater must not inherit v13 related inputs"
    );
    assert!(native.plan().shared_feedback().is_none());
    assert!(native.plan().standard_mode().is_none());
    assert!(!native.plan().fast_poll());
}
