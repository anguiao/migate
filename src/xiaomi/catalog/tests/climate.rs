use super::*;

#[test]
fn ac_models_keep_distinct_services_enums_and_integer_temperature_wire() {
    let mcn02 = compile_spec("lumi.acpartner.mcn02", &public_spec("lumi.acpartner.mcn02")).unwrap();
    let mcn04 = compile_spec("lumi.acpartner.mcn04", &public_spec("lumi.acpartner.mcn04")).unwrap();
    assert_eq!(mcn02.features[0].definition.service_instance, 2);
    assert_eq!(mcn04.features[0].definition.service_instance, 3);
    assert_eq!(
        mcn02.features[0]
            .binding
            .encode(&DeviceCommand::SetHvacMode(crate::device::HvacMode::Cool))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 2,
            value: WireValue::Integer(1)
        }]
    );
    assert_eq!(
        mcn04.features[0]
            .binding
            .encode(&DeviceCommand::SetHvacMode(crate::device::HvacMode::Cool))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 3,
            piid: 2,
            value: WireValue::Integer(0)
        }]
    );
    assert_eq!(
        mcn04.features[0]
            .binding
            .encode(&DeviceCommand::SetTargetTemperature(24.))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 3,
            piid: 4,
            value: WireValue::Integer(24)
        }]
    );
    assert!(
        mcn04.features[0]
            .binding
            .encode(&DeviceCommand::SetTargetTemperature(24.5))
            .is_err()
    );
    assert!(
        mcn04.features[0]
            .binding
            .encode(&DeviceCommand::SetTargetTemperature(31.))
            .is_err()
    );
}

#[test]
fn string_climate_fan_level_with_values_does_not_advertise_speed_control() {
    let mut malformed: serde_json::Value =
        serde_json::from_str(&public_spec("lumi.acpartner.mcn04")).unwrap();
    for service in malformed["services"].as_array_mut().unwrap() {
        if !service["type"]
            .as_str()
            .unwrap()
            .contains(":service:fan-control:")
        {
            continue;
        }
        for property in service["properties"].as_array_mut().unwrap() {
            let kind = property["type"].as_str().unwrap();
            if kind.contains(":property:fan-level:") {
                property["format"] = json!("string");
            }
        }
    }
    let compiled = compile_spec("lumi.acpartner.mcn04", &malformed.to_string()).unwrap();
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::Climate)
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

#[test]
fn empty_integer_climate_fan_level_does_not_advertise_speed_control() {
    let mut malformed: serde_json::Value =
        serde_json::from_str(&public_spec("lumi.acpartner.mcn04")).unwrap();
    for service in malformed["services"].as_array_mut().unwrap() {
        let Some(properties) = service["properties"].as_array_mut() else {
            continue;
        };
        for property in properties {
            if property["type"]
                .as_str()
                .unwrap()
                .contains(":property:fan-level:")
            {
                property["value-list"] = json!([]);
            }
        }
    }
    let compiled = compile_spec("lumi.acpartner.mcn04", &malformed.to_string()).unwrap();
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::Climate)
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

#[test]
fn non_boolean_climate_swing_does_not_advertise_swing_control() {
    let mut malformed: serde_json::Value =
        serde_json::from_str(&public_spec("lumi.acpartner.mcn04")).unwrap();
    for service in malformed["services"].as_array_mut().unwrap() {
        let Some(properties) = service["properties"].as_array_mut() else {
            continue;
        };
        for property in properties {
            if property["type"]
                .as_str()
                .unwrap()
                .contains(":property:vertical-swing:")
            {
                property["format"] = json!("string");
            }
        }
    }
    let compiled = compile_spec("lumi.acpartner.mcn04", &malformed.to_string()).unwrap();
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::Climate)
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
fn mcn02_legacy_mapping_exposes_reads_decodes_and_setters() {
    let legacy = Mcn02LegacyMapping::new();
    assert_eq!(
        legacy.read_fields(),
        &["power", "mode", "tar_temp", "fan_level", "ver_swing"]
    );
    assert_eq!(
        legacy.decode("mode", &WireValue::String("wind".into())),
        Some((
            Property::HvacMode,
            Some(PropertyValue::HvacMode(crate::device::HvacMode::FanOnly))
        ))
    );
    assert_eq!(
        legacy.decode("mode", &WireValue::String("unsupported".into())),
        Some((Property::HvacMode, None))
    );
    assert_eq!(
        legacy.encode(&DeviceCommand::SetFanSpeed(2)).unwrap(),
        LegacyMiioOperation {
            method: "set_fan_level",
            arguments: vec![WireValue::String("medium_fan".into())]
        }
    );
    assert_eq!(
        legacy
            .encode(&DeviceCommand::SetTargetTemperature(24.))
            .unwrap(),
        LegacyMiioOperation {
            method: "set_tar_temp",
            arguments: vec![WireValue::Integer(24)]
        }
    );
    assert!(
        legacy
            .encode(&DeviceCommand::SetTargetTemperature(24.5))
            .is_err()
    );
}

#[test]
fn climate_maps_only_explicit_own_service_room_temperature() {
    let climate_service = |include_room_temperature: bool| {
        let mut target = property(
            2,
            "target-temperature",
            "float",
            &["read", "write", "notify"],
        );
        target["unit"] = json!("celsius");
        target["value-range"] = json!([16, 30, 1]);
        let mut properties = vec![
            property(1, "on", "bool", &["read", "write", "notify"]),
            target,
        ];
        if include_room_temperature {
            let mut room = property(3, "temperature", "float", &["read", "notify"]);
            room["unit"] = json!("celsius");
            room["value-range"] = json!([-20, 60, 0.1]);
            properties.push(room);
        }
        service(2, "air-conditioner", Value::Array(properties))
    };
    let mut auxiliary = property(1, "temperature", "float", &["read", "notify"]);
    auxiliary["unit"] = json!("celsius");
    auxiliary["value-range"] = json!([-40, 125, 0.1]);
    let document = spec(
        "air-conditioner",
        json!([
            climate_service(true),
            service(8, "environment", json!([auxiliary.clone()])),
        ]),
    );
    let compiled = compile_spec("vendor.ac.room", &document).unwrap();
    let feature = &compiled.features[0];
    assert!(feature.definition.capabilities.0.iter().any(|capability| matches!(capability, Capability::Temperature(range) if range.minimum == -20. && range.maximum == 60.)));
    assert_eq!(
        feature.binding.decode(2, 3, &WireValue::Number(23.5)),
        Some((
            Property::CurrentTemperature,
            Some(PropertyValue::Temperature(23.5))
        ))
    );
    assert_eq!(feature.binding.decode(8, 1, &WireValue::Number(23.5)), None);

    let without_room = compile_spec(
        "vendor.ac.room",
        &spec(
            "air-conditioner",
            json!([
                climate_service(false),
                service(8, "environment", json!([auxiliary])),
            ]),
        ),
    )
    .unwrap();
    assert!(
        !without_room.features[0]
            .binding
            .properties
            .iter()
            .any(|mapping| mapping.property == Property::CurrentTemperature)
    );
    for model in ["lumi.acpartner.mcn02", "lumi.acpartner.mcn04"] {
        let compiled = compile_spec(model, &public_spec(model)).unwrap();
        assert!(
            !compiled.features[0]
                .binding
                .properties
                .iter()
                .any(|mapping| mapping.property == Property::CurrentTemperature)
        );
    }
}

#[test]
fn climate_temperature_wire_type_follows_spec_format() {
    temperature_wire_type_follows_spec_format("air-conditioner", "air-conditioner", "on");
}

#[test]
fn bath_temperature_wire_type_follows_spec_format() {
    temperature_wire_type_follows_spec_format("bath-heater", "ptc-bath-heater", "heating");
}

fn temperature_wire_type_follows_spec_format(device: &str, service_kind: &str, power: &str) {
    for format in [
        "uint8", "int8", "uint16", "int16", "uint32", "int32", "float",
    ] {
        let floating = format == "float";
        let mut target = property(
            2,
            "target-temperature",
            format,
            &["read", "write", "notify"],
        );
        target["unit"] = json!("celsius");
        target["value-range"] = json!([16, 30, if floating { 0.5 } else { 1.0 }]);
        let compiled = compile_spec(
            "vendor.climate.new",
            &spec(
                device,
                json!([service(
                    2,
                    service_kind,
                    json!([
                        property(1, power, "bool", &["read", "write", "notify"]),
                        target,
                    ])
                ),]),
            ),
        )
        .unwrap();
        let value = if floating { 22.5 } else { 22.0 };
        assert_eq!(
            compiled.features[0]
                .binding
                .encode(&DeviceCommand::SetTargetTemperature(value))
                .unwrap(),
            vec![WireOperation::SetProperty {
                siid: 2,
                piid: 2,
                value: if floating {
                    WireValue::Number(value)
                } else {
                    WireValue::Integer(22)
                },
            }],
            "{device} {format}",
        );
    }
}

#[test]
fn bath_heater_boolean_controls_require_boolean_wire_format() {
    let document = spec(
        "bath-heater",
        json!([service(
            3,
            "ptc-bath-heater",
            json!([
                property(2, "blow", "uint8", &["read", "write", "notify"]),
                property(3, "heating", "string", &["read", "write", "notify"]),
                property(4, "ventilation", "bool", &["read", "write", "notify"]),
            ])
        )]),
    );
    let compiled = compile_spec("vendor.bath.formats", &document).unwrap();
    assert_eq!(compiled.features.len(), 1);
    assert_eq!(
        compiled.features[0].definition.role,
        FeatureRole::BathHeaterExhaustFan
    );
}
