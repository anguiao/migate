use super::*;

#[test]
fn decoders_reject_invalid_wire_values_and_do_not_confirm_targets() {
    let curtain = compile_spec(
        "xiaomi.curtain.acn010",
        &public_spec("xiaomi.curtain.acn010"),
    )
    .unwrap();
    assert_eq!(
        curtain.features[0]
            .binding
            .decode(2, 4, &WireValue::Integer(50)),
        Some((
            Property::CurtainTargetPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    assert_eq!(
        curtain.features[0]
            .binding
            .decode(2, 3, &WireValue::Integer(50)),
        Some((
            Property::CurtainPosition,
            Some(PropertyValue::Percent(Percent::new(50.).unwrap()))
        ))
    );
    let fan = compile_spec("dmaker.fan.p5c", &public_spec("dmaker.fan.p5c")).unwrap();
    assert_eq!(
        fan.features[0].binding.decode(2, 2, &WireValue::Integer(9)),
        Some((Property::FanSpeed, None))
    );
    assert_eq!(
        fan.features[0]
            .binding
            .decode(2, 4, &WireValue::Boolean(true)),
        Some((
            Property::Oscillation,
            Some(PropertyValue::Oscillation(true))
        ))
    );
    let vacuum = compile_spec("xiaomi.vacuum.c104", &public_spec("xiaomi.vacuum.c104")).unwrap();
    assert_eq!(
        vacuum.features[0]
            .binding
            .decode(2, 1, &WireValue::Integer(10)),
        Some((
            Property::VacuumOperationalState,
            Some(PropertyValue::VacuumOperationalState(
                crate::device::VacuumOperationalState::Docked
            ))
        ))
    );
    assert_eq!(
        vacuum.features[0]
            .binding
            .decode(2, 1, &WireValue::Integer(3)),
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
        .find(|feature| feature.definition.role == FeatureRole::BathHeaterClimate)
        .unwrap();
    assert_eq!(
        climate.binding.decode(3, 8, &WireValue::Integer(46)),
        Some((Property::TargetTemperature, None))
    );
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
            .definition
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
                .definition
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
