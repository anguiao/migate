use super::*;
use crate::device::{
    Capability, DeviceCommand, FeatureRole, Percent, PresenceState, Property, PropertyValue,
    RgbColor, SensingModality,
};
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

fn spec(device: &str, services: serde_json::Value) -> String {
    json!({
        "type": format!("urn:miot-spec-v2:device:{device}:0000:test:1"),
        "description": "Test device",
        "services": services,
    })
    .to_string()
}

fn service(iid: u32, kind: &str, properties: serde_json::Value) -> serde_json::Value {
    json!({
        "iid": iid,
        "type": format!("urn:miot-spec-v2:service:{kind}:0000:test:1"),
        "description": kind,
        "properties": properties,
        "actions": [],
        "events": [],
    })
}

fn property(iid: u32, kind: &str, format: &str, access: &[&str]) -> serde_json::Value {
    json!({
        "iid": iid,
        "type": format!("urn:miot-spec-v2:property:{kind}:0000:test:1"),
        "description": kind,
        "format": format,
        "access": access,
    })
}

#[test]
fn compiles_standard_light_using_actual_range_and_wire_ids() {
    let mut brightness = property(2, "brightness", "uint16", &["read", "write", "notify"]);
    brightness["unit"] = json!("percentage");
    brightness["value-range"] = json!([1, 65535, 1]);
    let document = spec(
        "light",
        json!([
            service(1, "device-information", json!([])),
            service(
                2,
                "light",
                json!([
                    property(1, "on", "bool", &["read", "write", "notify"]),
                    brightness,
                ])
            ),
        ]),
    );
    let compiled = compile_spec("vendor.light.new", &document).unwrap();
    assert_eq!(compiled.features.len(), 1);
    let feature = &compiled.features[0];
    assert_eq!(feature.role, FeatureRole::Light);
    assert!(feature.capabilities.0.iter().any(|capability| matches!(capability, Capability::Brightness(range) if range.minimum == 0. && range.maximum == 100.)));
    assert_eq!(
        feature
            .encode(&DeviceCommand::SetBrightness(Percent::new(50.).unwrap()))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 2,
            value: WireValue::Integer(32768)
        }]
    );
    assert_eq!(
        feature
            .encode(&DeviceCommand::SetBrightness(Percent::new(0.).unwrap()))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 1,
            value: WireValue::Boolean(false)
        }]
    );
    assert_eq!(
        feature.decode(2, 2, &WireValue::Integer(1)),
        Some((
            Property::Brightness,
            Some(PropertyValue::Percent(Percent::new(100. / 65535.).unwrap()))
        ))
    );
    assert_eq!(
        feature
            .encode(&DeviceCommand::SetBrightness(Percent::new(100.).unwrap()))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 2,
            value: WireValue::Integer(65535)
        }]
    );
    let standard = compile_spec("yeelink.light.ml9", &public_spec("yeelink.light.ml9")).unwrap();
    assert_eq!(
        standard.features[0]
            .encode(&DeviceCommand::SetBrightness(Percent::new(50.).unwrap()))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 2,
            value: WireValue::Integer(50)
        }]
    );
}

#[test]
fn scope_exclusion_precedes_auxiliary_services_and_access_is_enforced() {
    let auxiliary = json!([service(
        2,
        "light",
        json!([property(1, "on", "bool", &["read", "write"]),])
    )]);
    assert!(
        compile_spec("yeelink.light.nl1", &spec("night-light", auxiliary.clone()))
            .unwrap()
            .features
            .is_empty()
    );
    assert!(
        compile_spec("vendor.camera.new", &spec("camera", auxiliary))
            .unwrap()
            .features
            .is_empty()
    );
    let read_only = spec(
        "light",
        json!([service(
            2,
            "light",
            json!([property(1, "on", "bool", &["read", "notify"]),])
        )]),
    );
    assert!(
        compile_spec("vendor.light.readonly", &read_only)
            .unwrap()
            .features
            .is_empty()
    );
}

#[test]
fn unknown_occupancy_values_decode_as_explicit_unknown() {
    let mut occupancy = property(1, "occupancy-status", "uint8", &["read", "notify"]);
    occupancy["value-list"] = json!([
        {"value": 0, "description": "No One"},
        {"value": 1, "description": "Has One"},
        {"value": 2, "description": "Quick Check"}
    ]);
    let compiled = compile_spec(
        "xiaomi.sensor_occupy.03",
        &spec(
            "occupancy-sensor",
            json!([service(2, "occupancy-sensor", json!([occupancy])),]),
        ),
    )
    .unwrap();
    let feature = &compiled.features[0];
    assert_eq!(
        feature.decode(2, 1, &WireValue::Integer(2)),
        Some((Property::Occupancy, None))
    );
    assert_eq!(
        feature.decode(2, 1, &WireValue::Integer(1)),
        Some((
            Property::Occupancy,
            Some(PropertyValue::Occupancy(PresenceState::Occupied))
        ))
    );
}

#[test]
fn indicator_and_protection_switches_do_not_create_loads() {
    let document = spec(
        "outlet",
        json!([
            service(
                2,
                "switch",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
            service(
                4,
                "indicator-light",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
            service(
                7,
                "power-protect",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
        ]),
    );
    let compiled = compile_spec("vendor.plug.new", &document).unwrap();
    assert_eq!(compiled.features.len(), 1);
    assert_eq!(compiled.features[0].service_instance, 2);
}

fn public_spec(model: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/miot_specs")
            .join(format!("{model}.json")),
    )
    .unwrap()
}

#[test]
fn all_approved_models_compile_the_expected_core_features() {
    let expectations = [
        ("yeelink.light.light3", 1),
        ("yeelink.light.ml9", 1),
        ("yeelink.light.spot2", 1),
        ("xiaomi.light.ceil04", 1),
        ("xiaomi.light.bar2", 1),
        ("devcea.light.ls2307", 1),
        ("lemesh.light.wy0c15", 1),
        ("mijia.light.group3", 1),
        ("xiaomi.switch.w3", 3),
        ("zimi.switch.dhkg01", 1),
        ("zimi.switch.dhkg02", 2),
        ("zimi.switch.dhkg05", 3),
        ("xiaomi.controller.86v1", 3),
        ("cuco.plug.cp7pd", 4),
        ("cuco.plug.v3", 1),
        ("qmi.plug.psv3", 1),
        ("zimi.plug.zncz01", 1),
        ("lumi.acpartner.mcn02", 1),
        ("lumi.acpartner.mcn04", 1),
        ("xiaomi.curtain.acn010", 1),
        ("dmaker.fan.p5c", 1),
        ("miaomiaoce.sensor_ht.t2", 2),
        ("miaomiaoce.sensor_ht.t6", 2),
        ("miaomiaoce.sensor_ht.t8", 2),
        ("miaomiaoce.sensor_ht.t9", 2),
        ("xiaomi.sensor_ht.mini", 2),
        ("xiaomi.motion.pir1", 2),
        ("izq.sensor_occupy.trio", 2),
        ("linp.sensor_occupy.hb01", 2),
        ("xiaomi.sensor_occupy.03", 2),
        ("xiaomi.sensor_occupy.p1", 2),
        ("isa.magnet.dw2hl", 1),
        ("linp.magnet.m1", 1),
        ("xiaomi.vacuum.c104", 1),
        ("yeelink.bhf_light.v13", 4),
    ];
    assert_eq!(expectations.len(), 35);
    for (model, count) in expectations {
        let compiled = compile_spec(model, &public_spec(model)).unwrap();
        assert_eq!(compiled.features.len(), count, "{model}");
    }
    for model in [
        "yeelink.light.light3",
        "yeelink.light.ml9",
        "yeelink.light.spot2",
        "xiaomi.light.ceil04",
        "xiaomi.light.bar2",
        "devcea.light.ls2307",
        "lemesh.light.wy0c15",
        "mijia.light.group3",
    ] {
        assert!(
            compile_spec(model, &public_spec(model))
                .unwrap()
                .features
                .iter()
                .all(|feature| feature.role == FeatureRole::Light),
            "{model}"
        );
    }
    for model in [
        "xiaomi.switch.w3",
        "zimi.switch.dhkg01",
        "zimi.switch.dhkg02",
        "zimi.switch.dhkg05",
        "xiaomi.controller.86v1",
        "cuco.plug.cp7pd",
        "cuco.plug.v3",
        "qmi.plug.psv3",
        "zimi.plug.zncz01",
    ] {
        assert!(
            compile_spec(model, &public_spec(model))
                .unwrap()
                .features
                .iter()
                .all(|feature| feature.role == FeatureRole::Load),
            "{model}"
        );
    }
    for model in ["lumi.acpartner.mcn02", "lumi.acpartner.mcn04"] {
        assert_eq!(
            compile_spec(model, &public_spec(model)).unwrap().features[0].role,
            FeatureRole::Climate,
            "{model}"
        );
    }
    for model in [
        "miaomiaoce.sensor_ht.t2",
        "miaomiaoce.sensor_ht.t6",
        "miaomiaoce.sensor_ht.t8",
        "miaomiaoce.sensor_ht.t9",
        "xiaomi.sensor_ht.mini",
    ] {
        let roles = compile_spec(model, &public_spec(model))
            .unwrap()
            .features
            .into_iter()
            .map(|feature| feature.role)
            .collect::<Vec<_>>();
        assert!(
            roles.contains(&FeatureRole::TemperatureSensor)
                && roles.contains(&FeatureRole::HumiditySensor),
            "{model}"
        );
    }
}

#[test]
fn high_risk_model_boundaries_follow_public_specs() {
    let panel = compile_spec(
        "xiaomi.controller.86v1",
        &public_spec("xiaomi.controller.86v1"),
    )
    .unwrap();
    assert_eq!(
        panel
            .features
            .iter()
            .map(|feature| feature.service_instance)
            .collect::<Vec<_>>(),
        vec![10, 11, 12]
    );

    let devcea = compile_spec("devcea.light.ls2307", &public_spec("devcea.light.ls2307")).unwrap();
    assert!(
        !devcea.features[0]
            .capabilities
            .0
            .contains(&Capability::Color)
    );
    assert!(
        devcea.features[0]
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Brightness(_)))
    );

    let p1 = compile_spec(
        "xiaomi.sensor_occupy.p1",
        &public_spec("xiaomi.sensor_occupy.p1"),
    )
    .unwrap();
    assert_eq!(p1.features[0].service_instance, 2);
    let isa = compile_spec("isa.magnet.dw2hl", &public_spec("isa.magnet.dw2hl")).unwrap();
    assert!(
        !isa.features[0]
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Illuminance(_)))
    );

    let bath = compile_spec(
        "yeelink.bhf_light.v13",
        &public_spec("yeelink.bhf_light.v13"),
    )
    .unwrap();
    let climate = bath
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::BathHeaterClimate)
        .unwrap();
    assert!(climate.capabilities.0.iter().any(|capability| matches!(capability, Capability::TargetTemperature(range) if range.minimum == 25. && range.maximum == 45.)));
    assert_eq!(
        climate.encode(&DeviceCommand::SetPower(false)).unwrap(),
        vec![WireOperation::SetProperty {
            siid: 3,
            piid: 3,
            value: WireValue::Boolean(false)
        }]
    );
}

#[test]
fn ac_models_keep_distinct_services_enums_and_integer_temperature_wire() {
    let mcn02 = compile_spec("lumi.acpartner.mcn02", &public_spec("lumi.acpartner.mcn02")).unwrap();
    let mcn04 = compile_spec("lumi.acpartner.mcn04", &public_spec("lumi.acpartner.mcn04")).unwrap();
    assert_eq!(mcn02.features[0].service_instance, 2);
    assert_eq!(mcn04.features[0].service_instance, 3);
    assert_eq!(
        mcn02.features[0]
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
            .encode(&DeviceCommand::SetTargetTemperature(24.5))
            .is_err()
    );
    assert!(
        mcn04.features[0]
            .encode(&DeviceCommand::SetTargetTemperature(31.))
            .is_err()
    );
}

#[test]
fn curtain_vacuum_and_fan_commands_use_real_discrete_endpoints() {
    let curtain = compile_spec(
        "xiaomi.curtain.acn010",
        &public_spec("xiaomi.curtain.acn010"),
    )
    .unwrap();
    assert_eq!(
        curtain.features[0]
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
            .capabilities
            .0
            .contains(&Capability::FanSpeeds(vec![1, 2, 3, 4]))
    );
    let vacuum = compile_spec("xiaomi.vacuum.c104", &public_spec("xiaomi.vacuum.c104")).unwrap();
    assert!(
        vacuum.features[0]
            .capabilities
            .0
            .contains(&Capability::VacuumDock)
    );
    assert_eq!(
        vacuum.features[0]
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
            .find(|feature| feature.role == FeatureRole::Fan)
            .unwrap();
        assert!(
            !feature
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
        .find(|feature| feature.role == FeatureRole::Fan)
        .unwrap();
    assert!(
        !feature
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
    assert!(feature.capabilities.0.contains(&Capability::VacuumControl));
    assert!(!feature.capabilities.0.contains(&Capability::VacuumDock));
    assert!(
        feature
            .capabilities
            .validate(&DeviceCommand::ReturnVacuumToDock)
            .is_err()
    );
    assert!(feature.encode(&DeviceCommand::ReturnVacuumToDock).is_err());
}

#[test]
fn motion_events_decode_arguments_and_model_absence_without_reading_config() {
    let compiled = compile_spec("xiaomi.motion.pir1", &public_spec("xiaomi.motion.pir1")).unwrap();
    assert_eq!(
        compiled
            .features
            .iter()
            .map(|feature| feature.role)
            .collect::<Vec<_>>(),
        [FeatureRole::MotionSensor, FeatureRole::IlluminanceSensor]
    );
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::MotionSensor)
        .unwrap();
    let illuminance = compiled
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::IlluminanceSensor)
        .unwrap();
    assert_eq!(feature.events.len(), 2);
    assert_eq!(
        feature.decode_event(2, 1008, &[WireValue::Number(12.5)]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(true)))])
    );
    assert_eq!(
        illuminance.decode_event(2, 1008, &[WireValue::Number(12.5)]),
        Some(vec![(
            Property::Illuminance,
            Some(PropertyValue::Illuminance(12.5))
        )])
    );
    assert_eq!(
        feature.decode_event(5, 1022, &[]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(false)))])
    );
    assert!(
        !feature
            .properties
            .iter()
            .any(|mapping| mapping.piid == 1053)
    );
    assert_eq!(
        feature.decode(2, 1024, &WireValue::Integer(120)),
        Some((Property::Motion, Some(PropertyValue::Motion(false))))
    );
    assert_eq!(
        feature.decode(2, 1024, &WireValue::Integer(0)),
        Some((Property::Motion, None))
    );
}

#[test]
fn approved_presence_models_publish_grounded_sensing_modalities() {
    for (model, expected) in [
        ("xiaomi.motion.pir1", vec![SensingModality::Pir]),
        ("linp.sensor_occupy.hb01", vec![SensingModality::Radar]),
        ("izq.sensor_occupy.trio", vec![SensingModality::Radar]),
        (
            "xiaomi.sensor_occupy.03",
            vec![SensingModality::Pir, SensingModality::Radar],
        ),
        (
            "xiaomi.sensor_occupy.p1",
            vec![SensingModality::Pir, SensingModality::Radar],
        ),
    ] {
        let compiled = compile_spec(model, &public_spec(model)).unwrap();
        assert_eq!(
            compiled.features[0].capabilities.sensing_modalities(),
            expected,
            "{model}"
        );
    }

    let mut occupancy = property(1, "occupancy-status", "uint8", &["read", "notify"]);
    occupancy["value-list"] = json!([
        {"value": 0, "description": "Vacant"},
        {"value": 1, "description": "Occupied"},
    ]);
    let generic = compile_spec(
        "vendor.sensor.future",
        &spec(
            "occupancy-sensor",
            json!([service(2, "occupancy-sensor", json!([occupancy]))]),
        ),
    )
    .unwrap();
    assert_eq!(
        generic.features[0].capabilities.sensing_modalities(),
        [SensingModality::Unspecified]
    );
}

#[test]
fn event_arguments_keep_wire_positions_when_unknown_parameters_are_present() {
    let mut ignored = property(7, "vendor-sequence", "uint8", &[]);
    ignored["value-range"] = json!([0, 255, 1]);
    let mut illumination = property(8, "illumination", "float", &[]);
    illumination["unit"] = json!("lux");
    illumination["value-range"] = json!([0, 1000, 0.1]);
    let mut motion = service(2, "motion-sensor", json!([ignored, illumination]));
    motion["events"] = json!([{
        "iid": 1,
        "type": "urn:miot-spec-v2:event:motion-detected:0000:test:1",
        "description": "Motion",
        "arguments": [7, 8]
    }]);
    let compiled =
        compile_spec("vendor.motion.new", &spec("motion-sensor", json!([motion]))).unwrap();
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::MotionSensor)
        .unwrap();
    let illuminance = compiled
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::IlluminanceSensor)
        .unwrap();
    assert_eq!(feature.events[0].argument_iids, vec![7, 8]);
    assert_eq!(illuminance.events[0].argument_iids, vec![7, 8]);
    assert!(
        illuminance
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Illuminance(_)))
    );
    assert_eq!(
        feature.decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(true)))])
    );
    assert_eq!(
        illuminance.decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)]),
        Some(vec![(
            Property::Illuminance,
            Some(PropertyValue::Illuminance(12.5))
        )])
    );
    assert_eq!(
        illuminance.decode_keyed_event(
            2,
            1,
            &[(8, WireValue::Number(12.5)), (7, WireValue::Integer(9))],
        ),
        illuminance.decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)])
    );
    assert!(
        feature
            .decode_keyed_event(
                2,
                1,
                &[(7, WireValue::Integer(9)), (7, WireValue::Integer(10))],
            )
            .is_none()
    );
    assert!(
        feature
            .decode_keyed_event(
                2,
                1,
                &[(7, WireValue::Integer(9)), (9, WireValue::Number(12.5))],
            )
            .is_none()
    );
    assert!(feature.properties.is_empty());
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
fn decoders_reject_invalid_wire_values_and_do_not_confirm_targets() {
    let curtain = compile_spec(
        "xiaomi.curtain.acn010",
        &public_spec("xiaomi.curtain.acn010"),
    )
    .unwrap();
    assert_eq!(
        curtain.features[0].decode(2, 4, &WireValue::Integer(50)),
        Some((
            Property::CurtainTargetPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    assert_eq!(
        curtain.features[0].decode(2, 3, &WireValue::Integer(50)),
        Some((
            Property::CurtainPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    let fan = compile_spec("dmaker.fan.p5c", &public_spec("dmaker.fan.p5c")).unwrap();
    assert_eq!(
        fan.features[0].decode(2, 2, &WireValue::Integer(9)),
        Some((Property::FanSpeed, None))
    );
    assert_eq!(
        fan.features[0].decode(2, 4, &WireValue::Boolean(true)),
        Some((
            Property::Oscillation,
            Some(PropertyValue::Oscillation(true))
        ))
    );
    let vacuum = compile_spec("xiaomi.vacuum.c104", &public_spec("xiaomi.vacuum.c104")).unwrap();
    assert_eq!(
        vacuum.features[0].decode(2, 1, &WireValue::Integer(10)),
        Some((
            Property::VacuumOperationalState,
            Some(PropertyValue::VacuumOperationalState(
                crate::device::VacuumOperationalState::Docked
            ))
        ))
    );
    assert_eq!(
        vacuum.features[0].decode(2, 1, &WireValue::Integer(3)),
        Some((
            Property::VacuumOperationalState,
            Some(PropertyValue::VacuumOperationalState(
                crate::device::VacuumOperationalState::Returning
            ))
        ))
    );

    let bath = compile_spec(
        "yeelink.bhf_light.v13",
        &public_spec("yeelink.bhf_light.v13"),
    )
    .unwrap();
    let climate = bath
        .features
        .iter()
        .find(|feature| feature.role == FeatureRole::BathHeaterClimate)
        .unwrap();
    assert_eq!(
        climate.decode(3, 8, &WireValue::Integer(46)),
        Some((Property::TargetTemperature, None))
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
            .capabilities
            .0
            .contains(&Capability::SwingModes(vec![
                crate::device::SwingMode::Off,
                crate::device::SwingMode::Vertical
            ]))
    );
}

#[test]
fn notify_only_sensor_is_supported_but_not_scheduled_for_read() {
    let mut temperature = property(1, "temperature", "float", &["notify"]);
    temperature["unit"] = json!("celsius");
    temperature["value-range"] = json!([-20, 60, 0.1]);
    let compiled = compile_spec(
        "vendor.sensor.new",
        &spec(
            "temperature-humidity-sensor",
            json!([service(
                2,
                "temperature-humidity-sensor",
                json!([temperature])
            ),]),
        ),
    )
    .unwrap();
    assert_eq!(compiled.features.len(), 1);
    assert!(!compiled.features[0].properties[0].readable);
    assert!(compiled.features[0].properties[0].notify);
}

#[test]
fn malformed_optional_arrays_and_wrong_units_are_rejected_or_ignored() {
    let malformed =
        json!({"type":"urn:miot-spec-v2:device:light:0000:test:1","services":"wrong"}).to_string();
    assert_eq!(
        compile_spec("vendor.light.new", &malformed),
        Err(CompileError::InvalidSpec)
    );
    let mut brightness = property(2, "brightness", "uint8", &["read", "write"]);
    brightness["unit"] = json!("celsius");
    brightness["value-range"] = json!([1, 100, 1]);
    let compiled = compile_spec(
        "vendor.light.new",
        &spec(
            "light",
            json!([service(
                2,
                "light",
                json!([property(1, "on", "bool", &["read", "write"]), brightness,])
            )]),
        ),
    )
    .unwrap();
    assert!(
        !compiled.features[0]
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Brightness(_)))
    );
}

#[test]
fn instance_ids_are_positive_and_unique_in_their_miot_namespaces() {
    let mut duplicate_services = serde_json::from_str::<Value>(&spec(
        "light",
        json!([
            service(
                2,
                "light",
                json!([property(1, "on", "bool", &["read", "write"])])
            ),
            service(2, "vendor", json!([])),
        ]),
    ))
    .unwrap();
    let mut zero_service = duplicate_services.clone();
    zero_service["services"][1]["iid"] = json!(0);
    duplicate_services["services"][1]["iid"] = json!(2);

    let mut duplicate_properties = service(
        2,
        "light",
        json!([
            property(1, "on", "bool", &["read", "write"]),
            property(1, "brightness", "uint8", &["read", "write"]),
        ]),
    );
    duplicate_properties["properties"][1]["unit"] = json!("percentage");
    duplicate_properties["properties"][1]["value-range"] = json!([1, 100, 1]);

    let operation_service = |actions: Value, events: Value| {
        let mut value = service(
            2,
            "light",
            json!([property(1, "on", "bool", &["read", "write"])]),
        );
        value["actions"] = actions;
        value["events"] = events;
        value
    };
    let operation = |iid: u32, kind: &str| {
        json!({
            "iid": iid,
            "type": format!("urn:miot-spec-v2:action:{kind}:0000:test:1"),
            "description": kind,
            "in": [1],
        })
    };
    let event = |iid: u32, kind: &str| {
        json!({
            "iid": iid,
            "type": format!("urn:miot-spec-v2:event:{kind}:0000:test:1"),
            "description": kind,
            "arguments": [1],
        })
    };
    let invalid = [
        duplicate_services,
        zero_service,
        serde_json::from_str(&spec("light", json!([duplicate_properties]))).unwrap(),
        serde_json::from_str(&spec(
            "light",
            json!([service(
                2,
                "light",
                json!([property(0, "on", "bool", &["read", "write"])]),
            )]),
        ))
        .unwrap(),
        serde_json::from_str(&spec(
            "light",
            json!([operation_service(
                json!([operation(2, "start"), operation(2, "stop")]),
                json!([]),
            )]),
        ))
        .unwrap(),
        serde_json::from_str(&spec(
            "light",
            json!([operation_service(
                json!([]),
                json!([event(2, "started"), event(2, "stopped")]),
            )]),
        ))
        .unwrap(),
        serde_json::from_str(&spec(
            "light",
            json!([operation_service(json!([operation(0, "start")]), json!([]),)]),
        ))
        .unwrap(),
        serde_json::from_str(&spec(
            "light",
            json!([operation_service(json!([]), json!([event(0, "started")]),)]),
        ))
        .unwrap(),
    ];
    for document in invalid {
        assert_eq!(
            compile_spec("vendor.light.ids", &document.to_string()),
            Err(CompileError::InvalidSpec)
        );
    }

    let same_id_in_distinct_namespaces = spec(
        "light",
        json!([operation_service(
            json!([operation(1, "start")]),
            json!([event(1, "started")]),
        )]),
    );
    assert!(compile_spec("vendor.light.ids", &same_id_in_distinct_namespaces).is_ok());
}

#[test]
fn supplied_property_metadata_has_valid_shapes_formats_and_ranges() {
    let light = |brightness: Value| {
        spec(
            "light",
            json!([service(
                2,
                "light",
                json!([property(1, "on", "bool", &["read", "write"]), brightness,])
            )]),
        )
    };
    let brightness = || property(2, "brightness", "uint8", &["read", "write"]);
    for (field, value) in [("unit", json!(42)), ("value-range", json!("0,100,1"))] {
        let mut malformed = brightness();
        malformed[field] = value;
        assert_eq!(
            compile_spec("vendor.light.metadata", &light(malformed)),
            Err(CompileError::InvalidSpec)
        );
    }
    for range in [json!([100, 1, 1]), json!([0, 100, 0]), json!([0, 100, -1])] {
        let mut malformed = brightness();
        malformed["unit"] = json!("percentage");
        malformed["value-range"] = range;
        assert_eq!(
            compile_spec("vendor.light.metadata", &light(malformed)),
            Err(CompileError::InvalidSpec)
        );
    }
    for format in ["bool", "string"] {
        let mut malformed = property(2, "brightness", format, &["read", "write"]);
        malformed["unit"] = json!("percentage");
        malformed["value-range"] = json!([0, 100, 1]);
        let compiled = compile_spec("vendor.light.metadata", &light(malformed)).unwrap();
        assert!(
            !compiled.features[0]
                .capabilities
                .0
                .iter()
                .any(|capability| matches!(capability, Capability::Brightness(_)))
        );
    }
    let mut optional_null = brightness();
    optional_null["unit"] = Value::Null;
    optional_null["value-range"] = Value::Null;
    assert!(compile_spec("vendor.light.metadata", &light(optional_null)).is_ok());
}

#[test]
fn curtain_current_and_target_positions_use_their_own_ranges() {
    let mut target = property(2, "target-position", "uint8", &["read", "write", "notify"]);
    target["unit"] = json!("percentage");
    target["value-range"] = json!([0, 100, 1]);
    let mut current = property(3, "current-position", "uint16", &["read", "notify"]);
    current["unit"] = json!("percentage");
    current["value-range"] = json!([0, 10000, 100]);
    let document = spec(
        "curtain",
        json!([service(2, "curtain", json!([target, current]))]),
    );
    let compiled = compile_spec("vendor.curtain.ranges", &document).unwrap();
    let feature = &compiled.features[0];
    assert_eq!(
        feature.decode(2, 3, &WireValue::Integer(5000)),
        Some((
            Property::CurtainPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    assert_eq!(
        feature.decode(2, 2, &WireValue::Integer(75)),
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
            .properties
            .iter()
            .any(|mapping| mapping.property == Property::CurtainPosition)
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
    assert!(feature.capabilities.0.iter().any(|capability| matches!(capability, Capability::Temperature(range) if range.minimum == -20. && range.maximum == 60.)));
    assert_eq!(
        feature.decode(2, 3, &WireValue::Number(23.5)),
        Some((
            Property::CurrentTemperature,
            Some(PropertyValue::Temperature(23.5))
        ))
    );
    assert_eq!(feature.decode(8, 1, &WireValue::Number(23.5)), None);

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
            .properties
            .iter()
            .any(|mapping| mapping.property == Property::CurrentTemperature)
    );
    for model in ["lumi.acpartner.mcn02", "lumi.acpartner.mcn04"] {
        let compiled = compile_spec(model, &public_spec(model)).unwrap();
        assert!(
            !compiled.features[0]
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
    assert_eq!(compiled.features[0].role, FeatureRole::BathHeaterExhaustFan);
}

#[test]
fn packed_rgb_requires_uint32_range_and_round_trips_without_truncation() {
    let mut color = property(3, "color", "uint32", &["read", "write", "notify"]);
    color["unit"] = json!("rgb");
    color["value-range"] = json!([0, 0x00ff_ffff, 1]);
    let valid = compile_spec(
        "vendor.light.rgb",
        &spec(
            "light",
            json!([service(
                2,
                "light",
                json!([
                    property(1, "on", "bool", &["read", "write", "notify"]),
                    color
                ])
            )]),
        ),
    )
    .unwrap();
    assert!(
        valid.features[0]
            .capabilities
            .0
            .contains(&Capability::Color)
    );
    let rgb = RgbColor {
        red: 0x12,
        green: 0x34,
        blue: 0x56,
    };
    assert_eq!(
        valid.features[0]
            .encode(&DeviceCommand::SetColor(rgb))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 3,
            value: WireValue::Integer(0x12_34_56)
        }]
    );
    assert_eq!(
        valid.features[0].decode(2, 3, &WireValue::Integer(0x12_34_56)),
        Some((Property::Color, Some(PropertyValue::Color(rgb))))
    );
    assert_eq!(
        valid.features[0].decode(2, 3, &WireValue::Integer(0x01_00_00_00)),
        Some((Property::Color, None))
    );

    let mut wrong = property(3, "color", "string", &["read", "write"]);
    wrong["unit"] = json!("rgb");
    wrong["value-range"] = json!([0, 0x00ff_ffff, 1]);
    let invalid = compile_spec(
        "vendor.light.invalid-rgb",
        &spec(
            "light",
            json!([service(
                2,
                "light",
                json!([property(1, "on", "bool", &["read", "write"]), wrong])
            )]),
        ),
    )
    .unwrap();
    assert!(
        !invalid.features[0]
            .capabilities
            .0
            .contains(&Capability::Color)
    );
}

#[test]
fn split_service_entries_merge_into_parent_while_groups_stay_independent() {
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome, OwnedRoom};
    let device = |did: &str, name: &str, model: &str, urn: &str| CloudDevice {
        did: did.into(),
        uid: Some("42".into()),
        name: name.into(),
        model: model.into(),
        spec_type: Some(urn.into()),
        pid: Some(8),
        token: None,
        online: None,
        local_ip: None,
        parent_id: None,
    };
    let light_urn =
        serde_json::from_str::<Value>(&public_spec("yeelink.light.light3")).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned();
    let group_urn =
        serde_json::from_str::<Value>(&public_spec("mijia.light.group3")).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned();
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["d1".into(), "d1.s2".into(), "group1".into()],
            rooms: vec![OwnedRoom {
                id: "r1".into(),
                name: "Room".into(),
                dids: vec!["d1".into(), "d1.s2".into()],
            }],
        }],
        devices: vec![
            device("d1", "Ceiling", "yeelink.light.light3", &light_urn),
            device(
                "d1.s2",
                "Bedside channel",
                "yeelink.light.light3",
                &light_urn,
            ),
            device("group1", "All lights", "mijia.light.group3", &group_urn),
        ],
    };
    let specs = [
        (light_urn.clone(), public_spec("yeelink.light.light3")),
        (group_urn.clone(), public_spec("mijia.light.group3")),
    ]
    .into_iter()
    .collect();
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 2);
    let parent = catalog
        .devices
        .iter()
        .find(|device| device.parent_did == "d1")
        .unwrap();
    assert_eq!(parent.features[0].name, "Bedside channel");
    assert_eq!(parent.room_id.as_deref(), Some("r1"));
    assert!(
        catalog
            .devices
            .iter()
            .any(|device| device.parent_did == "group1")
    );
}

fn child_only_cloud_catalog(
    home_did: &str,
) -> (crate::xiaomi::cloud::OwnedCatalog, HashMap<String, String>) {
    use crate::storage::DeviceToken;
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};
    let document = public_spec("yeelink.light.light3");
    let urn = serde_json::from_str::<Value>(&document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    (
        OwnedCatalog {
            uid: "42".into(),
            homes: vec![OwnedHome {
                id: "1".into(),
                name: "Home".into(),
                group_id: "0123456789abcdef".into(),
                dids: vec![home_did.into()],
                rooms: vec![],
            }],
            devices: vec![CloudDevice {
                did: "d1.s2".into(),
                uid: Some("42".into()),
                name: "Diagnostic child".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: Some(DeviceToken(vec![7; 16])),
                online: Some(true),
                local_ip: Some("192.168.1.2".into()),
                parent_id: Some("d1".into()),
            }],
        },
        [(urn, document)].into_iter().collect(),
    )
}

#[test]
fn parent_membership_keeps_child_only_detail_as_unrecognized_parent_candidate() {
    let (cloud, specs) = child_only_cloud_catalog("d1");
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    let candidate = &catalog.devices[0];
    assert_eq!(candidate.parent_did, "d1");
    assert_eq!(candidate.name, "Diagnostic child");
    assert!(candidate.features.is_empty());
    assert_eq!(candidate.token, None);
    assert_eq!(candidate.local_ip, None);
}

#[test]
fn child_membership_does_not_promote_child_spec_to_complete_parent_features() {
    let (cloud, specs) = child_only_cloud_catalog("d1.s2");
    let catalog = assemble_catalog(&cloud, &specs).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    assert!(catalog.devices[0].features.is_empty());
    assert_eq!(catalog.devices[0].token, None);
    assert_eq!(catalog.devices[0].local_ip, None);
}

#[test]
fn devices_without_specs_remain_diagnostic_candidates() {
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "group".into(),
            dids: vec!["legacy".into()],
            rooms: vec![],
        }],
        devices: vec![CloudDevice {
            did: "legacy".into(),
            uid: Some("42".into()),
            name: "Legacy".into(),
            model: "vendor.legacy.x".into(),
            spec_type: None,
            pid: None,
            token: None,
            online: None,
            local_ip: None,
            parent_id: None,
        }],
    };
    let catalog = assemble_catalog(&cloud, &HashMap::new()).unwrap();
    assert_eq!(catalog.devices.len(), 1);
    assert_eq!(catalog.devices[0].spec_type, None);
    assert!(catalog.devices[0].features.is_empty());
}

#[test]
fn catalog_debug_output_redacts_device_tokens() {
    use crate::storage::DeviceToken;
    let device = CatalogDevice {
        home_id: "1".into(),
        room_id: None,
        parent_did: "did".into(),
        name: "Device".into(),
        model: "vendor.device.x".into(),
        spec_type: None,
        pid: None,
        token: Some(DeviceToken(vec![0x11; 16])),
        online: None,
        local_ip: None,
        parent_id: None,
        features: vec![],
    };
    let debug = format!("{device:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("17, 17"));
}

#[test]
fn persisted_catalog_restores_compiled_mappings_and_split_names_after_logout_and_reopen() {
    use crate::storage::{Store, TokenSet, XiaomiRecord};
    use crate::xiaomi::cloud::{CloudDevice, OwnedCatalog, OwnedHome};

    let document = public_spec("yeelink.light.light3");
    let urn = serde_json::from_str::<Value>(&document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    let bath_document = public_spec("yeelink.bhf_light.v13");
    let bath_urn = serde_json::from_str::<Value>(&bath_document).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    let cloud = OwnedCatalog {
        uid: "42".into(),
        homes: vec![OwnedHome {
            id: "1".into(),
            name: "Home".into(),
            group_id: "0123456789abcdef".into(),
            dids: vec!["d1".into(), "d1.s2".into(), "bath".into(), "ignored".into()],
            rooms: vec![],
        }],
        devices: vec![
            CloudDevice {
                did: "d1".into(),
                uid: Some("42".into()),
                name: "Light".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: Some(false),
                local_ip: None,
                parent_id: None,
            },
            CloudDevice {
                did: "d1.s2".into(),
                uid: Some("42".into()),
                name: "Named channel".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: Some(false),
                local_ip: None,
                parent_id: Some("d1".into()),
            },
            CloudDevice {
                did: "bath".into(),
                uid: Some("42".into()),
                name: "Bath heater".into(),
                model: "yeelink.bhf_light.v13".into(),
                spec_type: Some(bath_urn.clone()),
                pid: Some(8),
                token: None,
                online: None,
                local_ip: None,
                parent_id: None,
            },
            CloudDevice {
                did: "ignored".into(),
                uid: Some("42".into()),
                name: "Deferred".into(),
                model: "yeelink.light.light3".into(),
                spec_type: Some(urn.clone()),
                pid: Some(8),
                token: None,
                online: None,
                local_ip: None,
                parent_id: None,
            },
        ],
    };
    let specs = [(urn, document), (bath_urn, bath_document)]
        .into_iter()
        .collect();
    let mut catalog = assemble_catalog(&cloud, &specs).unwrap();
    for feature in &mut catalog
        .devices
        .iter_mut()
        .find(|device| device.parent_did == "bath")
        .unwrap()
        .features
    {
        feature.name = format!("Custom {}", feature.role.as_str());
    }
    catalog
        .devices
        .iter_mut()
        .find(|device| device.parent_did == "ignored")
        .unwrap()
        .features
        .clear();
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&XiaomiRecord {
            uid: "42".into(),
            region: "cn".into(),
            oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
            redirect_uri: "http://homeassistant.local/callback".into(),
            tokens: TokenSet {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: 20,
                refresh_at: 10,
            },
            virtual_did: "1".into(),
            private_key_pem: "private".into(),
            certificate_pem: "certificate".into(),
        })
        .unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    assert!(persist_catalog(&store.devices(), &catalog, &specs, 10, revision).unwrap());
    store.xiaomi().logout().unwrap();
    drop(store);

    let reopened = Store::open(directory.path()).unwrap();
    let restored = restore_catalog(&reopened.devices(), "42").unwrap();
    assert_eq!(restored.homes, catalog.homes);
    assert_eq!(restored.devices, catalog.devices);
}
