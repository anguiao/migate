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
            horizontal_swing: true
        }
    );
    let fan = function.fan().unwrap();
    assert_eq!(fan.power(), PropertyBinding { siid: 2, piid: 1 });
    assert_eq!(fan.mode().property(), PropertyBinding { siid: 2, piid: 3 });
    assert_eq!(fan.mode().straight(), 0);
    assert_eq!(fan.speed().property(), PropertyBinding { siid: 8, piid: 1 });
    assert_eq!(fan.swing(), Some(PropertyBinding { siid: 2, piid: 4 }));
    assert_eq!(
        function.observation_bindings(),
        vec![
            fan.power(),
            fan.mode().property(),
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
            horizontal_swing: false
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
