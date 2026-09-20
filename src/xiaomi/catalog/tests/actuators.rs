use super::*;

#[test]
fn curtain_vacuum_and_fan_commands_use_real_discrete_endpoints() {
    let curtain = compile_spec(
        "xiaomi.curtain.acn010",
        &public_spec("xiaomi.curtain.acn010"),
    )
    .unwrap();
    assert_eq!(
        curtain.features[0]
            .binding
            .encode(&DeviceCommand::SetCurtainPosition(
                Percent::new(25.).unwrap()
            ))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 4,
            value: WireValue::Integer(25)
        }]
    );
    assert_eq!(
        curtain.features[0]
            .binding
            .encode(&DeviceCommand::StopCurtain)
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 1,
            value: WireValue::Integer(2)
        }]
    );
    let fan = compile_spec("dmaker.fan.p5c", &public_spec("dmaker.fan.p5c")).unwrap();
    assert!(
        fan.features[0]
            .definition
            .capabilities
            .0
            .contains(&Capability::FanSpeeds(vec![1, 2, 3, 4]))
    );
    let vacuum = compile_spec("xiaomi.vacuum.c104", &public_spec("xiaomi.vacuum.c104")).unwrap();
    assert!(
        vacuum.features[0]
            .definition
            .capabilities
            .0
            .contains(&Capability::VacuumDock)
    );
    assert_eq!(
        vacuum.features[0]
            .binding
            .encode(&DeviceCommand::ReturnVacuumToDock)
            .unwrap(),
        vec![WireOperation::InvokeAction {
            siid: 3,
            aiid: 1,
            input: vec![]
        }]
    );
}

#[test]
fn invalid_optional_fan_speed_does_not_advertise_empty_speed_capability() {
    for (format, remove_levels) in [("uint8", true), ("string", false)] {
        let mut document: serde_json::Value =
            serde_json::from_str(&public_spec("dmaker.fan.p5c")).unwrap();
        for service in document["services"].as_array_mut().unwrap() {
            if !service["type"].as_str().unwrap().contains(":service:fan:") {
                continue;
            }
            for property in service["properties"].as_array_mut().unwrap() {
                if property["type"]
                    .as_str()
                    .unwrap()
                    .contains(":property:fan-level:")
                {
                    property["format"] = json!(format);
                    if remove_levels {
                        property.as_object_mut().unwrap().remove("value-range");
                        property.as_object_mut().unwrap().remove("value-list");
                    }
                }
            }
        }
        let compiled = compile_spec("dmaker.fan.p5c", &document.to_string()).unwrap();
        let feature = compiled
            .features
            .iter()
            .find(|feature| feature.definition.role == FeatureRole::Fan)
            .unwrap();
        assert!(
            !feature
                .definition
                .capabilities
                .0
                .iter()
                .any(|capability| matches!(capability, Capability::FanSpeeds(_)))
        );
    }
}

#[test]
fn non_boolean_swing_does_not_advertise_boolean_control() {
    let mut wrong_swing: serde_json::Value =
        serde_json::from_str(&public_spec("dmaker.fan.p5c")).unwrap();
    for service in wrong_swing["services"].as_array_mut().unwrap() {
        for property in service["properties"].as_array_mut().unwrap() {
            if property["type"]
                .as_str()
                .unwrap()
                .contains(":property:horizontal-swing:")
            {
                property["format"] = json!("string");
            }
        }
    }
    let compiled = compile_spec("dmaker.fan.p5c", &wrong_swing.to_string()).unwrap();
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::Fan)
        .unwrap();
    assert!(
        !feature
            .definition
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::SwingModes(_)))
    );
}

#[test]
fn vacuum_actions_require_empty_inputs_and_dock_capability_is_independent() {
    let mut parameterized = service(2, "vacuum", json!([]));
    parameterized["actions"] = json!([
        {
            "iid": 1,
            "type": "urn:miot-spec-v2:action:start-sweep:0000:test:1",
            "description": "Start",
            "in": [1]
        },
        {
            "iid": 2,
            "type": "urn:miot-spec-v2:action:stop-sweeping:0000:test:1",
            "description": "Stop",
            "in": [1]
        }
    ]);
    parameterized["properties"] = json!([property(1, "room", "uint8", &[])]);
    let compiled = compile_spec(
        "vendor.vacuum.parameterized",
        &spec("vacuum", json!([parameterized])),
    )
    .unwrap();
    assert!(compiled.features.is_empty());

    let mut no_dock = service(2, "vacuum", json!([]));
    no_dock["actions"] = json!([
        {
            "iid": 1,
            "type": "urn:miot-spec-v2:action:start-sweep:0000:test:1",
            "description": "Start",
            "in": []
        },
        {
            "iid": 2,
            "type": "urn:miot-spec-v2:action:stop-sweeping:0000:test:1",
            "description": "Stop",
            "in": []
        }
    ]);
    let compiled =
        compile_spec("vendor.vacuum.no-dock", &spec("vacuum", json!([no_dock]))).unwrap();
    let feature = &compiled.features[0];
    assert!(
        feature
            .definition
            .capabilities
            .0
            .contains(&Capability::VacuumControl)
    );
    assert!(
        !feature
            .definition
            .capabilities
            .0
            .contains(&Capability::VacuumDock)
    );
    assert!(
        feature
            .definition
            .capabilities
            .validate(&DeviceCommand::ReturnVacuumToDock)
            .is_err()
    );
    assert!(
        feature
            .binding
            .encode(&DeviceCommand::ReturnVacuumToDock)
            .is_err()
    );
}

#[test]
fn vacuum_fault_decoder_honors_integer_format_and_declared_range() {
    let vacuum = compile_spec("xiaomi.vacuum.c104", &public_spec("xiaomi.vacuum.c104")).unwrap();
    let feature = &vacuum.features[0];

    for value in [
        WireValue::String("0".into()),
        WireValue::Number(1.0),
        WireValue::Integer(-1),
        WireValue::Integer(3001),
    ] {
        assert_eq!(
            feature.binding.decode(2, 2, &value),
            Some((Property::VacuumFault, None))
        );
    }
    assert_eq!(
        feature.binding.decode(2, 2, &WireValue::Integer(0)),
        Some((
            Property::VacuumFault,
            Some(PropertyValue::VacuumFault("0".into()))
        ))
    );
    assert_eq!(
        feature.binding.decode(2, 2, &WireValue::Integer(3000)),
        Some((
            Property::VacuumFault,
            Some(PropertyValue::VacuumFault("3000".into()))
        ))
    );

    let mut document: Value = serde_json::from_str(&public_spec("xiaomi.vacuum.c104")).unwrap();
    let fault = document["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 2)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 2)
        .unwrap();
    fault.as_object_mut().unwrap().remove("value-range");
    fault["value-list"] = json!([
        {"value": 0, "description": "No Fault"},
        {"value": 17, "description": "Vendor Fault"}
    ]);
    let enum_vacuum = compile_spec("xiaomi.vacuum.c104", &document.to_string()).unwrap();
    assert_eq!(
        enum_vacuum.features[0]
            .binding
            .decode(2, 2, &WireValue::Integer(17)),
        Some((
            Property::VacuumFault,
            Some(PropertyValue::VacuumFault("17".into()))
        ))
    );
    assert_eq!(
        enum_vacuum.features[0]
            .binding
            .decode(2, 2, &WireValue::Integer(18)),
        Some((Property::VacuumFault, None))
    );
    let mut ranged: Value = serde_json::from_str(&public_spec("xiaomi.vacuum.c104")).unwrap();
    let fault = ranged["services"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|service| service["iid"] == 2)
        .unwrap()["properties"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|property| property["iid"] == 2)
        .unwrap();
    fault["value-range"] = json!([0, 3000, 2]);
    let ranged = compile_spec("xiaomi.vacuum.c104", &ranged.to_string()).unwrap();
    assert_eq!(
        ranged.features[0]
            .binding
            .decode(2, 2, &WireValue::Integer(17)),
        Some((Property::VacuumFault, None))
    );
    assert_eq!(
        ranged.features[0]
            .binding
            .decode(2, 2, &WireValue::Integer(18)),
        Some((
            Property::VacuumFault,
            Some(PropertyValue::VacuumFault("18".into()))
        ))
    );
}

#[test]
fn generic_vertical_fan_reports_vertical_swing() {
    let mut invalid_horizontal = property(
        2,
        "horizontal-swing",
        "string",
        &["read", "write", "notify"],
    );
    invalid_horizontal["description"] = json!("Invalid horizontal swing");
    let mut swing = property(3, "vertical-swing", "bool", &["read", "write", "notify"]);
    swing["description"] = json!("Vertical swing");
    let document = spec(
        "fan",
        json!([service(
            2,
            "fan",
            json!([
                property(1, "on", "bool", &["read", "write", "notify"]),
                invalid_horizontal,
                swing
            ])
        )]),
    );
    let compiled = compile_spec("vendor.fan.vertical", &document).unwrap();
    assert!(
        compiled.features[0]
            .definition
            .capabilities
            .0
            .contains(&Capability::SwingModes(vec![
                crate::device::SwingMode::Off,
                crate::device::SwingMode::Vertical
            ]))
    );
}

#[test]
fn curtain_current_and_target_positions_use_their_own_ranges() {
    let mut target = property(2, "target-position", "uint8", &["read", "write", "notify"]);
    target["unit"] = json!("percentage");
    target["value-range"] = json!([0, 100, 5]);
    let mut current = property(3, "current-position", "uint16", &["read", "notify"]);
    current["unit"] = json!("percentage");
    current["value-range"] = json!([0, 10000, 100]);
    let document = spec(
        "curtain",
        json!([service(2, "curtain", json!([target, current]))]),
    );
    let compiled = compile_spec("vendor.curtain.ranges", &document).unwrap();
    let feature = &compiled.features[0];
    assert!(feature.definition.capabilities.0.iter().any(|capability| {
        matches!(capability, Capability::CurtainPosition(range) if *range == crate::device::NumericRange {
            minimum: 0.0,
            maximum: 100.0,
            step: 5.0,
            unit: crate::device::NumericUnit::Percent,
        })
    }));
    assert_eq!(
        feature.binding.decode(2, 3, &WireValue::Integer(5000)),
        Some((
            Property::CurtainPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    assert_eq!(
        feature.binding.decode(2, 2, &WireValue::Integer(75)),
        Some((
            Property::CurtainTargetPosition,
            Some(PropertyValue::Percent(Percent::new(75.).unwrap()))
        ))
    );

    let mut missing_current = serde_json::from_str::<Value>(&document).unwrap();
    missing_current["services"][0]["properties"][1]
        .as_object_mut()
        .unwrap()
        .remove("value-range");
    let compiled = compile_spec("vendor.curtain.ranges", &missing_current.to_string()).unwrap();
    assert!(
        !compiled.features[0]
            .binding
            .properties
            .iter()
            .any(|mapping| mapping.property == Property::CurtainPosition)
    );
}

#[test]
fn curtain_rejects_target_ranges_that_cannot_normalize_to_percent() {
    for range in [json!([0, 0, 1]), json!([-100, -1, 1])] {
        let mut target = property(2, "target-position", "int16", &["read", "write", "notify"]);
        target["unit"] = json!("percentage");
        target["value-range"] = range;
        let document = spec("curtain", json!([service(2, "curtain", json!([target]))]));
        assert!(
            compile_spec("vendor.curtain.invalid-range", &document)
                .unwrap()
                .features
                .is_empty()
        );
    }
}
