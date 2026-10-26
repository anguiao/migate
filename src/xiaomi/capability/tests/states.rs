//! Read-only contact, motion and whole-device occupancy capabilities.
use super::*;
use crate::xiaomi::capability::parse_device_for_model;

#[test]
fn state_samples_expose_independent_stable_functions() {
    for (model, text, expected) in [
        (
            "isa.magnet.dw2hl",
            include_str!("../../../../tests/fixtures/miot/isa-dw2hl.json"),
            vec!["contact"],
        ),
        (
            "xiaomi.motion.pir1",
            include_str!("../../../../tests/fixtures/miot/xiaomi-pir1.json"),
            vec!["illuminance", "motion"],
        ),
        (
            "izq.sensor_occupy.trio",
            include_str!("../../../../tests/fixtures/miot/izq-trio.json"),
            vec!["illuminance", "occupancy"],
        ),
        (
            "linp.sensor_occupy.hb01",
            include_str!("../../../../tests/fixtures/miot/linp-hb01.json"),
            vec!["illuminance", "occupancy"],
        ),
        (
            "xiaomi.sensor_occupy.03",
            include_str!("../../../../tests/fixtures/miot/xiaomi-occupy-03.json"),
            vec!["illuminance", "occupancy"],
        ),
        (
            "xiaomi.sensor_occupy.p1",
            include_str!("../../../../tests/fixtures/miot/xiaomi-occupy-p1.json"),
            vec!["illuminance", "occupancy"],
        ),
    ] {
        let instance: Value = serde_json::from_str(text).unwrap();
        let device =
            parse_device_for_model(DeviceKey::new("xiaomi", "state"), model, &instance).unwrap();
        assert_eq!(
            device
                .functions()
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>(),
            expected,
            "{model}"
        );
        assert!(device.is_read_only());
    }
}

#[test]
fn state_native_values_follow_explicit_semantics() {
    let parse = |text| {
        let value: Value = serde_json::from_str(text).unwrap();
        parse_device_for_model(DeviceKey::new("xiaomi", "state"), "sample", &value).unwrap()
    };
    let contact = parse(include_str!(
        "../../../../tests/fixtures/miot/isa-dw2hl.json"
    ));
    let binding = contact.functions()[0].state().unwrap();
    assert_eq!(binding.property(), PropertyBinding { siid: 2, piid: 2 });
    assert_eq!(binding.read_value(&json!(true)), Ok(Some(false)));
    assert_eq!(binding.read_value(&json!(false)), Ok(Some(true)));
    assert!(binding.read_value(&json!(1)).is_err());
    let motion = parse(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ));
    let function = motion
        .functions()
        .iter()
        .find(|f| f.id() == "motion")
        .unwrap();
    let binding = function.state().unwrap();
    assert_eq!(
        binding.property(),
        PropertyBinding {
            siid: 2,
            piid: 1024
        }
    );
    assert_eq!(
        function.motion().unwrap().event(),
        super::super::EventBinding {
            siid: 2,
            eiid: 1008
        }
    );
    for (raw, active) in [(0, true), (1, false), (120, false), (300, false)] {
        assert_eq!(binding.read_value(&json!(raw)), Ok(Some(active)));
    }
    for raw in [json!(-1), json!(0.5), json!(false)] {
        assert!(binding.read_value(&raw).is_err());
    }
    for text in [
        include_str!("../../../../tests/fixtures/miot/linp-hb01.json"),
        include_str!("../../../../tests/fixtures/miot/xiaomi-occupy-03.json"),
    ] {
        let parsed = parse(text);
        let binding = parsed
            .functions()
            .iter()
            .find(|f| f.id() == "occupancy")
            .unwrap()
            .state()
            .unwrap();
        assert_eq!(binding.read_value(&json!(0)), Ok(Some(false)));
        assert_eq!(binding.read_value(&json!(1)), Ok(Some(true)));
        assert_eq!(binding.read_value(&json!(2)), Ok(None));
        assert!(binding.read_value(&json!(100)).is_err());
    }
}

#[test]
fn state_capabilities_require_real_access_and_motion_recovery() {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    value["services"][1]["events"] = json!([]);
    let device =
        parse_device_for_model(DeviceKey::new("xiaomi", "motion"), "sample", &value).unwrap();
    assert_eq!(
        device
            .functions()
            .iter()
            .map(|f| f.id())
            .collect::<Vec<_>>(),
        ["illuminance"]
    );
    let mut value: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-occupy-p1.json"
    ))
    .unwrap();
    value["services"][1]["properties"][0]["access"] = json!(["read"]);
    let device =
        parse_device_for_model(DeviceKey::new("xiaomi", "occupancy"), "sample", &value).unwrap();
    assert!(device.functions().iter().all(|f| f.id() != "occupancy"));
}

#[test]
fn enumerated_contact_uses_description_values_and_prefers_boolean_contact() {
    let mut instance: Value =
        serde_json::from_str(include_str!("../../../../tests/fixtures/miot/linp-m1.json")).unwrap();
    instance["services"][1]["properties"][0]["value-list"][0]["value"] = json!(41);
    instance["services"][1]["properties"][0]["value-list"][1]["value"] = json!(42);
    let parse = |instance: &Value| {
        parse_device_for_model(DeviceKey::new("xiaomi", "contact"), "sample", instance).unwrap()
    };
    let parsed = parse(&instance);
    let contact = parsed.functions()[0].state().unwrap();
    assert_eq!(
        contact.property(),
        PropertyBinding {
            siid: 2,
            piid: 1004
        }
    );
    assert_eq!(contact.read_value(&json!(41)), Ok(Some(true)));
    assert_eq!(contact.read_value(&json!(42)), Ok(Some(false)));
    assert!(contact.read_value(&json!(1)).is_err());
    instance["services"][1]["properties"].as_array_mut().unwrap().push(json!({"iid":7,"type":"urn:miot-spec-v2:property:contact-state:0000007C:sample:1","format":"bool","access":["read","notify"]}));
    let parsed = parse(&instance);
    assert_eq!(
        parsed.functions()[0].state().unwrap().property(),
        PropertyBinding { siid: 2, piid: 7 }
    );
    instance["services"][1]["properties"][1]["access"] = json!(["notify"]);
    let parsed = parse(&instance);
    assert_eq!(
        parsed.functions()[0].state().unwrap().property(),
        PropertyBinding {
            siid: 2,
            piid: 1004
        }
    );
}

#[test]
fn motion_retains_event_arguments_and_requires_the_matching_recovery_property() {
    let source: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-pir1.json"
    ))
    .unwrap();
    let parse = |source: &Value| {
        parse_device_for_model(DeviceKey::new("xiaomi", "motion"), "sample", source).unwrap()
    };
    let parsed = parse(&source);
    let event = parsed
        .functions()
        .iter()
        .find(|function| function.id() == "motion")
        .unwrap()
        .motion()
        .unwrap();
    assert_eq!(
        event.arguments(),
        &[super::super::EventArgument {
            property: PropertyBinding {
                siid: 2,
                piid: 1005
            },
            format: "float".into(),
            unit: Some("lux".into())
        }]
    );
    for (field, value) in [
        ("access", json!(["read"])),
        ("access", json!(["notify"])),
        ("format", json!("float")),
        ("unit", json!("minutes")),
    ] {
        let mut changed = source.clone();
        changed["services"][1]["properties"][0][field] = value;
        let parsed = parse(&changed);
        assert_eq!(
            parsed
                .functions()
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>(),
            ["illuminance"]
        );
    }
    let p1: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/miot/xiaomi-occupy-p1.json"
    ))
    .unwrap();
    let parsed = parse(&p1);
    assert_eq!(
        parsed
            .functions()
            .iter()
            .find(|function| function.id() == "occupancy")
            .unwrap()
            .state()
            .unwrap()
            .property(),
        PropertyBinding { siid: 2, piid: 1 }
    );
}
