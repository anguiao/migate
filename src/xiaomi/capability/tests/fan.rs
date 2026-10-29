use crate::{
    device::{DeviceKey, FunctionKind},
    xiaomi::capability::{PropertyBinding, parse_device_for_model},
};
use serde_json::{Value, json};

fn instance() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/dmaker-p5c.json"
    ))
    .unwrap()
}
fn parse(value: &Value) -> Result<super::super::ParsedDevice, super::super::CapabilityError> {
    parse_device_for_model(DeviceKey::new("xiaomi", "fan"), "dmaker.fan.p5c", value)
}
fn property(value: &mut Value, siid: u32, piid: u32) -> &mut Value {
    value["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == siid)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == piid)
        .unwrap()
}

#[test]
fn public_fan_description_selects_confirmed_native_inputs() {
    let parsed = parse(&instance()).unwrap();
    assert_eq!(parsed.functions().len(), 1);
    let function = &parsed.functions()[0];
    assert_eq!(function.id(), "main");
    assert_eq!(
        function.kind(),
        FunctionKind::Fan {
            horizontal_swing: true,
            speed_count: 100
        }
    );
    let fan = function.fan().unwrap();
    assert_eq!(fan.power(), PropertyBinding { siid: 2, piid: 1 });
    assert_eq!(
        fan.mode().unwrap().property(),
        PropertyBinding { siid: 2, piid: 3 }
    );
    assert_eq!(fan.mode().unwrap().straight(), 0);
    assert_eq!(fan.speed().property(), PropertyBinding { siid: 8, piid: 1 });
    assert_eq!(fan.swing(), Some(PropertyBinding { siid: 2, piid: 4 }));
    assert_eq!(
        function.observation_bindings(),
        vec![
            fan.power(),
            fan.mode().unwrap().property(),
            fan.speed().property(),
            fan.swing().unwrap()
        ]
    );
    assert!(fan.speed().read_value(&json!(0)).is_err());
    assert_eq!(fan.speed().read_value(&json!(100)).unwrap(), 100);
}

#[test]
fn fan_requires_confirmed_semantics_permissions_format_and_range() {
    for (siid, piid) in [(2, 1), (2, 3), (8, 1)] {
        for permission in ["read", "write", "notify"] {
            let mut value = instance();
            property(&mut value, siid, piid)["access"] = json!(
                ["read", "write", "notify"]
                    .into_iter()
                    .filter(|item| *item != permission)
                    .collect::<Vec<_>>()
            );
            assert!(parse(&value).is_err(), "{siid}/{piid} {permission}");
        }
    }
    for (siid, piid, field, bad) in [
        (2, 1, "format", json!("uint8")),
        (2, 3, "format", json!("float")),
        (8, 1, "format", json!("float")),
        (8, 1, "value-range", json!([0, 100, 1])),
        (8, 1, "value-range", json!([1, 100, 2])),
        (
            8,
            1,
            "type",
            json!("urn:other-spec:property:speed-level:00000001:dmaker-p5c:1"),
        ),
    ] {
        let mut value = instance();
        property(&mut value, siid, piid)[field] = bad;
        assert!(parse(&value).is_err(), "{siid}/{piid} {field}");
    }
    let mut value = instance();
    property(&mut value, 2, 3)["value-list"][0]["description"] = json!("Other");
    assert!(parse(&value).is_err());
    let mut value = instance();
    property(&mut value, 2, 3)["value-list"][1]["value"] = json!(0);
    assert!(parse(&value).is_err());
}

#[test]
fn invalid_or_absent_swing_is_not_published() {
    let mut value = instance();
    property(&mut value, 2, 4)["access"] = json!(["read"]);
    let parsed = parse(&value).unwrap();
    assert_eq!(
        parsed.functions()[0].kind(),
        FunctionKind::Fan {
            horizontal_swing: false,
            speed_count: 100
        }
    );
    assert_eq!(parsed.functions()[0].fan().unwrap().swing(), None);
}

#[test]
fn fan_admission_rejects_a_mismatched_model_manufacturer() {
    assert!(
        parse_device_for_model(
            DeviceKey::new("xiaomi", "fan"),
            "other.fan.p5c",
            &instance()
        )
        .is_err()
    );
}

fn generic_fan(speeds: Vec<Value>) -> Value {
    let mut properties = vec![json!({
        "iid": 1,
        "type": "urn:miot-spec-v2:property:on:00000006:example-fan:1",
        "format": "bool",
        "access": ["read", "write", "notify"]
    })];
    properties.extend(speeds);
    json!({
        "type": "urn:miot-spec-v2:device:fan:0000A005:example-fan:1",
        "services": [{
            "iid": 2,
            "type": "urn:miot-spec-v2:service:fan:00007808:example-fan:1",
            "properties": properties
        }]
    })
}

fn generic_speed(iid: u32, shape: Value) -> Value {
    let mut property = json!({
        "iid": iid,
        "type": "urn:miot-spec-v2:property:fan-level:00000016:example-fan:1",
        "format": "uint16",
        "access": ["read", "write", "notify"]
    });
    property
        .as_object_mut()
        .unwrap()
        .extend(shape.as_object().unwrap().clone());
    property
}

fn parse_generic(
    value: &Value,
) -> Result<super::super::ParsedDevice, super::super::CapabilityError> {
    parse_device_for_model(DeviceKey::new("xiaomi", "fan"), "example.fan.model", value)
}

#[test]
fn generic_ranges_map_reachable_raw_values_to_ordered_speeds() {
    for (range, raw_and_speed) in [
        (json!([10, 30, 10]), vec![(10, 1), (20, 2), (30, 3)]),
        (json!([0, 8, 2]), vec![(0, 1), (2, 2), (8, 5)]),
        (json!([1, 100, 1]), vec![(1, 1), (33, 33), (100, 100)]),
    ] {
        let device = parse_generic(&generic_fan(vec![generic_speed(
            2,
            json!({"value-range": range}),
        )]))
        .unwrap();
        let speed = device.functions()[0].fan().unwrap().speed();
        for (raw, expected) in raw_and_speed {
            assert_eq!(speed.read_value(&json!(raw)).unwrap(), expected);
        }
        assert!(speed.read_value(&json!(999)).is_err());
    }
}

#[test]
fn generic_discrete_levels_follow_semantic_order_not_code_or_list_order() {
    let device = parse_generic(&generic_fan(vec![generic_speed(
        2,
        json!({"value-list": [
            {"value": 42, "description": "Level3"},
            {"value": 0, "description": "Level1"},
            {"value": 7, "description": "Level2"}
        ]}),
    )]))
    .unwrap();
    let speed = device.functions()[0].fan().unwrap().speed();
    assert_eq!(speed.read_value(&json!(0)).unwrap(), 1);
    assert_eq!(speed.read_value(&json!(7)).unwrap(), 2);
    assert_eq!(speed.read_value(&json!(42)).unwrap(), 3);
    assert!(speed.read_value(&json!(1)).is_err());
}

#[test]
fn generic_speed_selection_skips_unexplained_mixed_and_invalid_inputs() {
    let mixed = generic_speed(
        2,
        json!({"value-list": [
            {"value": 0, "description": "Auto"},
            {"value": 1, "description": "Level1"},
            {"value": 2, "description": "Level2"}
        ]}),
    );
    assert!(parse_generic(&generic_fan(vec![mixed.clone()])).is_err());
    let mut invalid = generic_speed(3, json!({"value-range": [1, 100, 1]}));
    invalid["access"] = json!(["read", "write"]);
    assert!(parse_generic(&generic_fan(vec![invalid.clone()])).is_err());
    let valid = generic_speed(4, json!({"value-range": [10, 30, 10]}));
    let device = parse_generic(&generic_fan(vec![mixed, invalid, valid])).unwrap();
    assert_eq!(
        device.functions()[0].fan().unwrap().speed().property().piid,
        4
    );
}

#[test]
fn generic_speed_selection_prefers_more_levels_and_rejects_ties_and_overflow() {
    let fewer = generic_speed(2, json!({"value-range": [1, 3, 1]}));
    let more = generic_speed(3, json!({"value-range": [1, 5, 1]}));
    let device = parse_generic(&generic_fan(vec![fewer.clone(), more])).unwrap();
    assert_eq!(
        device.functions()[0].fan().unwrap().speed().property().piid,
        3
    );
    let tied = generic_speed(4, json!({"value-range": [10, 30, 10]}));
    assert!(parse_generic(&generic_fan(vec![fewer, tied])).is_err());
    let too_many = generic_speed(2, json!({"value-range": [0, 100, 1]}));
    let error = parse_generic(&generic_fan(vec![too_many])).unwrap_err();
    assert!(error.to_string().contains("fan speed"), "{error}");
}

#[test]
fn dmaker_without_a_mode_can_use_an_independent_standard_speed() {
    let mut value = generic_fan(vec![generic_speed(2, json!({"value-range": [1, 4, 1]}))]);
    value["type"] = json!("urn:miot-spec-v2:device:fan:0000A005:dmaker-other:1");
    let parsed =
        parse_device_for_model(DeviceKey::new("xiaomi", "fan"), "dmaker.fan.other", &value)
            .unwrap();
    let fan = parsed.functions()[0].fan().unwrap();
    assert!(fan.mode().is_none());
    assert_eq!(fan.speed().property(), PropertyBinding { siid: 2, piid: 2 });
    assert_eq!(fan.speed().count(), 4);
}

#[test]
fn generic_speed_requires_standard_type_permissions_and_unambiguous_ordinary_levels() {
    for permission in ["read", "write", "notify"] {
        let mut speed = generic_speed(2, json!({"value-range": [1, 3, 1]}));
        speed["access"] = json!(
            ["read", "write", "notify"]
                .into_iter()
                .filter(|item| *item != permission)
                .collect::<Vec<_>>()
        );
        assert!(
            parse_generic(&generic_fan(vec![speed])).is_err(),
            "{permission}"
        );
    }
    for special in ["Auto", "Natural Wind", "Sleep", "unknown"] {
        let speed = generic_speed(
            2,
            json!({"value-list": [
                {"value": 8, "description": "Level1"}, {"value": 20, "description": special}
            ]}),
        );
        assert!(
            parse_generic(&generic_fan(vec![speed])).is_err(),
            "{special}"
        );
    }
    let mut nonstandard = generic_speed(2, json!({"value-range": [1, 3, 1]}));
    nonstandard["type"] = json!("urn:example-spec:property:fan-level:00000016:example-fan:1");
    assert!(parse_generic(&generic_fan(vec![nonstandard])).is_err());
    let single = generic_speed(2, json!({"value-range": [0, 0, 1]}));
    let parsed = parse_generic(&generic_fan(vec![single])).unwrap();
    let speed = parsed.functions()[0].fan().unwrap().speed();
    assert_eq!(speed.count(), 1);
    assert_eq!(speed.encode(1), 0);
    assert_eq!(speed.read_value(&json!(0)).unwrap(), 1);
}

#[test]
fn generic_fan_admission_does_not_depend_on_model_prefix() {
    let value = generic_fan(vec![generic_speed(2, json!({"value-range": [1, 4, 1]}))]);
    let parsed = parse_device_for_model(
        DeviceKey::new("xiaomi", "fan"),
        "dmaker.fan.generic",
        &value,
    )
    .unwrap();
    assert!(parsed.functions()[0].fan().unwrap().mode().is_none());
}

#[test]
fn dmaker_with_unrelated_mode_does_not_require_the_straight_wind_patch() {
    let mut value = generic_fan(vec![generic_speed(2, json!({"value-range": [1, 4, 1]}))]);
    value["type"] = json!("urn:miot-spec-v2:device:fan:0000A005:dmaker-other:1");
    value["services"][0]["properties"].as_array_mut().unwrap().push(json!({
        "iid": 3,
        "type": "urn:miot-spec-v2:property:mode:00000008:dmaker-other:1",
        "format": "uint8",
        "access": ["read", "write", "notify"],
        "value-list": [{"value": 0, "description": "Ordinary"}, {"value": 1, "description": "Other"}]
    }));
    value["services"].as_array_mut().unwrap().push(json!({
        "iid": 8,
        "type": "urn:dmaker-spec:service:dm-service:00007802:dmaker-other:1",
        "properties": []
    }));
    let parsed =
        parse_device_for_model(DeviceKey::new("xiaomi", "fan"), "dmaker.fan.other", &value)
            .unwrap();
    let fan = parsed.functions()[0].fan().unwrap();
    assert!(fan.mode().is_none());
    assert_eq!(fan.speed().count(), 4);
}
