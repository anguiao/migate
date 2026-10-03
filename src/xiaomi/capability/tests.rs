use super::*;
use serde_json::{Value, json};

fn key() -> DeviceKey {
    DeviceKey::new("xiaomi", "test-outlet")
}

fn public_description() -> Value {
    // Source: https://miot-spec.org/miot-spec-v2/instance?type=urn:miot-spec-v2:device:outlet:0000A002:cuco-v3:2
    serde_json::from_str(include_str!("../../../tests/fixtures/miot/outlet.json")).unwrap()
}

fn minimal_description() -> Value {
    json!({
        "type": "urn:miot-spec-v2:device:outlet:0000A002:other-vendor:7",
        "services": [{
            "iid": 42,
            "type": "urn:miot-spec-v2:service:switch:0000780C:other-vendor:4",
            "properties": [{
                "iid": 19,
                "type": "urn:miot-spec-v2:property:on:00000006:other-vendor:3",
                "format": "bool",
                "access": ["notify", "write", "read"]
            }]
        }]
    })
}

#[test]
fn public_spec_selects_main_switch_and_ignores_indicator_and_vendor_switches() {
    let outlet = parse_outlet(key(), &public_description()).unwrap();
    assert_eq!(outlet.description.key, key());
    assert_eq!(outlet.description.category, Category::Outlet);
    assert_eq!(outlet.description.capabilities, [Capability::OnOff]);
    assert_eq!(outlet.main_power, PropertyBinding { siid: 2, piid: 1 });
}

#[test]
fn standard_semantics_work_with_other_vendor_and_instance_ids() {
    let outlet = parse_outlet(key(), &minimal_description()).unwrap();
    assert_eq!(outlet.main_power, PropertyBinding { siid: 42, piid: 19 });
}

#[test]
fn descriptions_and_instance_order_do_not_determine_the_main_power() {
    let mut description = public_description();
    description["services"].as_array_mut().unwrap().reverse();
    for service in description["services"].as_array_mut().unwrap() {
        service["description"] = json!("Switch");
    }
    assert_eq!(
        parse_outlet(key(), &description).unwrap().main_power,
        PropertyBinding { siid: 2, piid: 1 }
    );
}

#[test]
fn indicator_light_without_a_functional_switch_is_unsupported() {
    let mut description = minimal_description();
    description["services"][0]["type"] =
        json!("urn:miot-spec-v2:service:indicator-light:00007803:other-vendor:1");
    assert_eq!(
        parse_outlet(key(), &description),
        Err(CapabilityError::MissingMainPower)
    );
}

#[test]
fn non_boolean_main_power_and_each_missing_permission_are_rejected() {
    let mut description = minimal_description();
    description["services"][0]["properties"][0]["format"] = json!("uint8");
    assert_eq!(
        parse_outlet(key(), &description),
        Err(CapabilityError::InvalidMainPowerType)
    );
    for permission in ["read", "write", "notify"] {
        let mut description = minimal_description();
        description["services"][0]["properties"][0]["access"] = json!(
            ["read", "write", "notify"]
                .into_iter()
                .filter(|value| *value != permission)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_outlet(key(), &description),
            Err(CapabilityError::MissingAccess(permission))
        );
    }
}

#[test]
fn multiple_functional_power_properties_are_ambiguous() {
    let mut description = minimal_description();
    let mut second = description["services"][0]["properties"][0].clone();
    second["iid"] = json!(20);
    description["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(second);
    assert_eq!(
        parse_outlet(key(), &description),
        Err(CapabilityError::AmbiguousMainPower)
    );

    let mut description = minimal_description();
    let mut second = description["services"][0].clone();
    second["iid"] = json!(43);
    description["services"].as_array_mut().unwrap().push(second);
    assert_eq!(
        parse_outlet(key(), &description),
        Err(CapabilityError::AmbiguousMainPower)
    );
}

#[test]
fn private_or_inconsistent_urns_cannot_masquerade_as_standard_power() {
    for urn in [
        "urn:other-spec:property:on:00000006:other-vendor:1",
        "urn:miot-spec-v2:property:on:000000FF:other-vendor:1",
        "urn:miot-spec-v2:property:unknown:00000006:other-vendor:1",
        "on",
    ] {
        let mut description = minimal_description();
        description["services"][0]["properties"][0]["type"] = json!(urn);
        assert_eq!(
            parse_outlet(key(), &description),
            Err(CapabilityError::MissingMainPower)
        );
    }
}

#[test]
fn unsupported_devices_and_malformed_protocol_fields_fail_at_the_boundary() {
    let mut description = minimal_description();
    description["type"] = json!("urn:miot-spec-v2:device:light:0000A001:other-vendor:1");
    assert_eq!(
        parse_outlet(key(), &description),
        Err(CapabilityError::UnsupportedDevice)
    );
    for value in [
        json!(null),
        json!({"type": "urn:miot-spec-v2:device:outlet:0000A002:other-vendor:1", "services": null}),
    ] {
        assert!(matches!(
            parse_outlet(key(), &value),
            Err(CapabilityError::InvalidDescription(_))
        ));
    }
    for iid in [json!(0), json!(-1), json!("2"), json!(u64::MAX)] {
        let mut description = minimal_description();
        description["services"][0]["iid"] = iid;
        assert!(matches!(
            parse_outlet(key(), &description),
            Err(CapabilityError::InvalidDescription(_))
        ));
    }
    let mut description = minimal_description();
    description["services"][0]["properties"][0]["access"] = json!(["read", 1, "write", "notify"]);
    assert!(matches!(
        parse_outlet(key(), &description),
        Err(CapabilityError::InvalidDescription(_))
    ));
}
