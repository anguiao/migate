use super::{CapabilityError, PropertyBinding, parse_device};
use crate::device::{Capability, Category, DeviceKey};
use serde_json::{Value, json};

fn key() -> DeviceKey {
    DeviceKey::new("xiaomi", "test-outlet")
}

fn public_description() -> Value {
    // Source: https://miot-spec.org/miot-spec-v2/instance?type=urn:miot-spec-v2:device:outlet:0000A002:cuco-v3:2
    serde_json::from_str(include_str!("../../../../tests/fixtures/miot/outlet.json")).unwrap()
}

fn model_description(model: &str) -> Value {
    // Fixtures are unmodified public MIoT instance descriptions from miot-spec.org.
    let fixture = match model {
        "cuco.plug.v3" => include_str!("../../../../tests/fixtures/miot/outlet.json"),
        "cuco.plug.cp7pd" => include_str!("../../../../tests/fixtures/miot/cuco-cp7pd.json"),
        "xiaomi.plug.04" => include_str!("../../../../tests/fixtures/miot/xiaomi-04.json"),
        "zimi.plug.zncz01" => include_str!("../../../../tests/fixtures/miot/zimi-zncz01.json"),
        "qmi.plug.psv3" => include_str!("../../../../tests/fixtures/miot/qmi-psv3.json"),
        _ => panic!("unknown test model"),
    };
    serde_json::from_str(fixture).unwrap()
}

#[test]
fn four_single_outlet_models_accept_their_public_instance_urns() {
    for model in [
        "cuco.plug.v3",
        "xiaomi.plug.04",
        "zimi.plug.zncz01",
        "qmi.plug.psv3",
    ] {
        let outlet = parse_device(key(), &model_description(model))
            .unwrap_or_else(|error| panic!("{model}: {error}"));
        assert_eq!(
            outlet.functions[0].power,
            PropertyBinding { siid: 2, piid: 1 }
        );
        let device = parse_device(key(), &model_description(model)).unwrap();
        assert_eq!(device.functions.len(), 1);
        assert_eq!(device.functions[0].description.id, "main");
        assert_eq!(device.functions[0].power, outlet.functions[0].power);
        assert_eq!(device.refresh_trigger, None);
    }
}

#[test]
fn cp7pd_exposes_three_independent_channels_and_observes_total_control_internally() {
    let description = model_description("cuco.plug.cp7pd");
    let device = parse_device(key(), &description).unwrap();
    assert_eq!(device.key, key());
    assert_eq!(device.functions[0].description.category, Category::Outlet);
    assert_eq!(
        device.functions[0].description.capabilities,
        [Capability::OnOff]
    );
    assert_eq!(
        device
            .functions
            .iter()
            .map(|function| (
                function.description.id.as_str(),
                function.description.label.as_str(),
                function.power
            ))
            .collect::<Vec<_>>(),
        [
            ("outlet_1", "开关 1", PropertyBinding { siid: 3, piid: 1 }),
            ("outlet_2", "开关 2", PropertyBinding { siid: 4, piid: 1 }),
            ("usb", "USB", PropertyBinding { siid: 9, piid: 1 }),
        ]
    );
    assert_eq!(
        device.refresh_trigger,
        Some(PropertyBinding { siid: 2, piid: 1 })
    );
    assert!(
        device
            .functions
            .iter()
            .all(|function| function.power.siid != 2)
    );
}

#[test]
fn cp7pd_mapping_is_independent_of_descriptions_and_list_order() {
    let original = model_description("cuco.plug.cp7pd");
    let mut reversed = original.clone();
    reversed["services"].as_array_mut().unwrap().reverse();
    for service in reversed["services"].as_array_mut().unwrap() {
        service["description"] = json!("Unknown");
        if let Some(properties) = service["properties"].as_array_mut() {
            properties.reverse();
            for property in properties {
                property["description"] = json!("Unknown");
            }
        }
    }
    assert_eq!(
        parse_device(key(), &original),
        parse_device(key(), &reversed)
    );
}

fn cp7pd_service_mut(description: &mut Value, siid: u32) -> &mut Value {
    description["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == siid)
        .unwrap()
}

#[test]
fn cp7pd_requires_all_channels_and_the_internal_total_control_binding() {
    for (siid, id) in [(2, "master"), (3, "outlet_1"), (4, "outlet_2"), (9, "usb")] {
        let mut description = model_description("cuco.plug.cp7pd");
        description["services"]
            .as_array_mut()
            .unwrap()
            .retain(|service| service["iid"] != siid);
        assert_eq!(
            parse_device(key(), &description),
            Err(CapabilityError::MissingFunction(id))
        );

        let mut description = model_description("cuco.plug.cp7pd");
        cp7pd_service_mut(&mut description, siid)["properties"]
            .as_array_mut()
            .unwrap()
            .retain(|property| property["iid"] != 1);
        assert_eq!(
            parse_device(key(), &description),
            Err(CapabilityError::MissingFunction(id))
        );
    }
}

#[test]
fn cp7pd_checks_boolean_type_and_every_permission_for_each_binding() {
    for siid in [2, 3, 4, 9] {
        let mut description = model_description("cuco.plug.cp7pd");
        cp7pd_service_mut(&mut description, siid)["properties"][0]["format"] = json!("uint8");
        assert_eq!(
            parse_device(key(), &description),
            Err(CapabilityError::InvalidMainPowerType)
        );
        for permission in ["read", "write", "notify"] {
            let mut description = model_description("cuco.plug.cp7pd");
            cp7pd_service_mut(&mut description, siid)["properties"][0]["access"] = json!(
                ["read", "write", "notify"]
                    .into_iter()
                    .filter(|value| *value != permission)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                parse_device(key(), &description),
                Err(CapabilityError::MissingAccess(permission))
            );
        }
    }
}

#[test]
fn cp7pd_rejects_wrong_vendor_namespace_semantics_and_description_version() {
    for (siid, field, urn) in [
        (
            3,
            "service",
            "urn:other-spec:service:switch:00007801:cuco-cp7pd:1",
        ),
        (
            4,
            "service",
            "urn:cuco-spec:service:switch:00007801:cuco-cp7pd:1",
        ),
        (
            3,
            "service",
            "urn:cuco-spec:service:switch:00007801:other-cp7pd:1",
        ),
        (
            3,
            "service",
            "urn:cuco-spec:service:indicator-light:00007801:cuco-cp7pd:1",
        ),
        (
            3,
            "property",
            "urn:miot-spec-v2:property:on:00000006:cuco-cp7pd:1",
        ),
        (
            3,
            "property",
            "urn:cuco-spec:property:on:00000001:other-cp7pd:1",
        ),
        (
            3,
            "property",
            "urn:cuco-spec:property:cyc-status:00000001:cuco-cp7pd:1",
        ),
        (
            9,
            "service",
            "urn:miot-spec-v2:service:indicator-light:00007803:cuco-cp7pd:1",
        ),
        (
            9,
            "property",
            "urn:miot-spec-v2:property:on:00000006:cuco-cp7pd:2",
        ),
    ] {
        let mut description = model_description("cuco.plug.cp7pd");
        let service = cp7pd_service_mut(&mut description, siid);
        if field == "service" {
            service["type"] = json!(urn);
        } else {
            service["properties"][0]["type"] = json!(urn);
        }
        assert!(
            matches!(
                parse_device(key(), &description),
                Err(CapabilityError::IncompatibleModel(_))
            ),
            "{urn}"
        );
    }
    let mut description = model_description("cuco.plug.cp7pd");
    description["type"] = json!("urn:miot-spec-v2:device:outlet:0000A002:cuco-cp7pd:2");
    assert!(matches!(
        parse_device(key(), &description),
        Err(CapabilityError::IncompatibleModel(_))
    ));
    description["type"] = json!("urn:miot-spec-v2:device:outlet:0000A002:other-cp7pd:1");
    assert_eq!(
        parse_device(key(), &description),
        Err(CapabilityError::AmbiguousMainPower)
    );
}

#[test]
fn urn_suffix_requires_one_eight_digit_hexadecimal_field() {
    for tail in [
        ":",
        ":garbage",
        ":0000C81",
        ":0000C816F",
        ":0000G816",
        ":0000C816:more",
    ] {
        for field in ["device", "service", "property"] {
            let mut description = minimal_description();
            let target = match field {
                "device" => &mut description["type"],
                "service" => &mut description["services"][0]["type"],
                _ => &mut description["services"][0]["properties"][0]["type"],
            };
            *target = json!(format!("{}{tail}", target.as_str().unwrap()));
            assert!(
                matches!(
                    parse_device(key(), &description),
                    Err(CapabilityError::InvalidDescription(_))
                ),
                "{field} {tail}"
            );
        }
    }
    for tail in [":0000C816", ":0000d032"] {
        let mut description = minimal_description();
        description["type"] = json!(format!("{}{tail}", description["type"].as_str().unwrap()));
        description["services"][0]["type"] = json!(format!(
            "{}{tail}",
            description["services"][0]["type"].as_str().unwrap()
        ));
        description["services"][0]["properties"][0]["type"] = json!(format!(
            "{}{tail}",
            description["services"][0]["properties"][0]["type"]
                .as_str()
                .unwrap()
        ));
        assert!(parse_device(key(), &description).is_ok());
    }
}

#[test]
fn duplicate_service_and_property_ids_are_invalid_at_the_description_boundary() {
    let mut description = minimal_description();
    let duplicate = description["services"][0].clone();
    description["services"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(matches!(
        parse_device(key(), &description),
        Err(CapabilityError::InvalidDescription(_))
    ));
    let mut description = minimal_description();
    let duplicate = description["services"][0]["properties"][0].clone();
    description["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(matches!(
        parse_device(key(), &description),
        Err(CapabilityError::InvalidDescription(_))
    ));
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
    let outlet = parse_device(key(), &public_description()).unwrap();
    assert_eq!(outlet.key, key());
    assert_eq!(outlet.functions[0].description.category, Category::Outlet);
    assert_eq!(
        outlet.functions[0].description.capabilities,
        [Capability::OnOff]
    );
    assert_eq!(
        outlet.functions[0].power,
        PropertyBinding { siid: 2, piid: 1 }
    );
}

#[test]
fn standard_semantics_work_with_other_vendor_and_instance_ids() {
    let outlet = parse_device(key(), &minimal_description()).unwrap();
    assert_eq!(
        outlet.functions[0].power,
        PropertyBinding { siid: 42, piid: 19 }
    );
}

#[test]
fn descriptions_and_instance_order_do_not_determine_the_main_power() {
    let mut description = public_description();
    description["services"].as_array_mut().unwrap().reverse();
    for service in description["services"].as_array_mut().unwrap() {
        service["description"] = json!("Switch");
    }
    assert_eq!(
        parse_device(key(), &description).unwrap().functions[0].power,
        PropertyBinding { siid: 2, piid: 1 }
    );
}

#[test]
fn indicator_light_without_a_functional_switch_is_unsupported() {
    let mut description = minimal_description();
    description["services"][0]["type"] =
        json!("urn:miot-spec-v2:service:indicator-light:00007803:other-vendor:1");
    assert_eq!(
        parse_device(key(), &description),
        Err(CapabilityError::MissingMainPower)
    );
}

#[test]
fn non_boolean_main_power_and_each_missing_permission_are_rejected() {
    let mut description = minimal_description();
    description["services"][0]["properties"][0]["format"] = json!("uint8");
    assert_eq!(
        parse_device(key(), &description),
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
            parse_device(key(), &description),
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
        parse_device(key(), &description),
        Err(CapabilityError::AmbiguousMainPower)
    );

    let mut description = minimal_description();
    let mut second = description["services"][0].clone();
    second["iid"] = json!(43);
    description["services"].as_array_mut().unwrap().push(second);
    assert_eq!(
        parse_device(key(), &description),
        Err(CapabilityError::AmbiguousMainPower)
    );
}

#[test]
fn private_or_inconsistent_urns_cannot_masquerade_as_standard_power() {
    for urn in [
        "urn:other-spec:property:on:00000006:other-vendor:1",
        "urn:miot-spec-v2:property:on:000000FF:other-vendor:1",
        "urn:miot-spec-v2:property:unknown:00000006:other-vendor:1",
    ] {
        let mut description = minimal_description();
        description["services"][0]["properties"][0]["type"] = json!(urn);
        assert_eq!(
            parse_device(key(), &description),
            Err(CapabilityError::MissingMainPower)
        );
    }
    let mut description = minimal_description();
    description["services"][0]["properties"][0]["type"] = json!("on");
    assert_eq!(
        parse_device(key(), &description),
        Err(CapabilityError::InvalidDescription("type"))
    );
}

#[test]
fn unsupported_devices_and_malformed_protocol_fields_fail_at_the_boundary() {
    let mut description = minimal_description();
    description["type"] = json!("urn:miot-spec-v2:device:light:0000A001:other-vendor:1");
    assert_eq!(
        parse_device(key(), &description),
        Err(CapabilityError::UnsupportedDevice)
    );
    for value in [
        json!(null),
        json!({"type": "urn:miot-spec-v2:device:outlet:0000A002:other-vendor:1", "services": null}),
    ] {
        assert!(matches!(
            parse_device(key(), &value),
            Err(CapabilityError::InvalidDescription(_))
        ));
    }
    for iid in [json!(0), json!(-1), json!("2"), json!(u64::MAX)] {
        let mut description = minimal_description();
        description["services"][0]["iid"] = iid;
        assert!(matches!(
            parse_device(key(), &description),
            Err(CapabilityError::InvalidDescription(_))
        ));
    }
    let mut description = minimal_description();
    description["services"][0]["properties"][0]["access"] = json!(["read", 1, "write", "notify"]);
    assert!(matches!(
        parse_device(key(), &description),
        Err(CapabilityError::InvalidDescription(_))
    ));
}

mod group;
mod lighting;
mod repair;
