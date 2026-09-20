use super::*;

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
        feature.binding.decode(2, 1, &WireValue::Integer(2)),
        Some((Property::Occupancy, None))
    );
    assert_eq!(
        feature.binding.decode(2, 1, &WireValue::Integer(1)),
        Some((
            Property::Occupancy,
            Some(PropertyValue::Occupancy(PresenceState::Occupied))
        ))
    );
}

#[test]
fn motion_events_decode_arguments_and_model_absence_without_reading_config() {
    let compiled = compile_spec("xiaomi.motion.pir1", &public_spec("xiaomi.motion.pir1")).unwrap();
    assert_eq!(
        compiled
            .features
            .iter()
            .map(|feature| feature.definition.role)
            .collect::<Vec<_>>(),
        [FeatureRole::MotionSensor, FeatureRole::IlluminanceSensor]
    );
    let feature = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::MotionSensor)
        .unwrap();
    let illuminance = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::IlluminanceSensor)
        .unwrap();
    assert_eq!(feature.binding.events.len(), 2);
    assert_eq!(
        feature
            .binding
            .decode_event(2, 1008, &[WireValue::Number(12.5)]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(true)))])
    );
    assert_eq!(
        illuminance
            .binding
            .decode_event(2, 1008, &[WireValue::Number(12.5)]),
        Some(vec![(
            Property::Illuminance,
            Some(PropertyValue::Illuminance(12.5))
        )])
    );
    assert_eq!(
        feature.binding.decode_event(5, 1022, &[]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(false)))])
    );
    assert!(
        !feature
            .binding
            .properties
            .iter()
            .any(|mapping| mapping.piid == 1053)
    );
    assert_eq!(
        feature.binding.decode(2, 1024, &WireValue::Integer(120)),
        Some((Property::Motion, Some(PropertyValue::Motion(false))))
    );
    assert_eq!(
        feature.binding.decode(2, 1024, &WireValue::Integer(0)),
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
            compiled.features[0]
                .definition
                .capabilities
                .sensing_modalities(),
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
        generic.features[0]
            .definition
            .capabilities
            .sensing_modalities(),
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
        .find(|feature| feature.definition.role == FeatureRole::MotionSensor)
        .unwrap();
    let illuminance = compiled
        .features
        .iter()
        .find(|feature| feature.definition.role == FeatureRole::IlluminanceSensor)
        .unwrap();
    assert_eq!(feature.binding.events[0].argument_iids, vec![7, 8]);
    assert_eq!(illuminance.binding.events[0].argument_iids, vec![7, 8]);
    assert!(
        illuminance
            .definition
            .capabilities
            .0
            .iter()
            .any(|capability| matches!(capability, Capability::Illuminance(_)))
    );
    assert_eq!(
        feature
            .binding
            .decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)]),
        Some(vec![(Property::Motion, Some(PropertyValue::Motion(true)))])
    );
    assert_eq!(
        illuminance
            .binding
            .decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)]),
        Some(vec![(
            Property::Illuminance,
            Some(PropertyValue::Illuminance(12.5))
        )])
    );
    assert_eq!(
        illuminance.binding.decode_keyed_event(
            2,
            1,
            &[(8, WireValue::Number(12.5)), (7, WireValue::Integer(9))],
        ),
        illuminance
            .binding
            .decode_event(2, 1, &[WireValue::Integer(9), WireValue::Number(12.5)])
    );
    assert!(
        feature
            .binding
            .decode_keyed_event(
                2,
                1,
                &[(7, WireValue::Integer(9)), (7, WireValue::Integer(10))],
            )
            .is_none()
    );
    assert!(
        feature
            .binding
            .decode_keyed_event(
                2,
                1,
                &[(7, WireValue::Integer(9)), (9, WireValue::Number(12.5))],
            )
            .is_none()
    );
    assert!(feature.binding.properties.is_empty());
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
    assert!(!compiled.features[0].binding.properties[0].readable);
    assert!(compiled.features[0].binding.properties[0].notify);
}
