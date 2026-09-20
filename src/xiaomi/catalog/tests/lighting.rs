use super::*;

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
    assert_eq!(feature.definition.role, FeatureRole::Light);
    assert!(feature.definition.capabilities.0.iter().any(|capability| matches!(capability, Capability::Brightness(range) if range.minimum == 0. && range.maximum == 100.)));
    assert_eq!(
        feature
            .binding
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
            .binding
            .encode(&DeviceCommand::SetBrightness(Percent::new(0.).unwrap()))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 1,
            value: WireValue::Boolean(false)
        }]
    );
    assert_eq!(
        feature.binding.decode(2, 2, &WireValue::Integer(1)),
        Some((
            Property::Brightness,
            Some(PropertyValue::Percent(Percent::new(100. / 65535.).unwrap()))
        ))
    );
    assert_eq!(
        feature
            .binding
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
            .binding
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
            .definition
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
            .binding
            .encode(&DeviceCommand::SetColor(rgb))
            .unwrap(),
        vec![WireOperation::SetProperty {
            siid: 2,
            piid: 3,
            value: WireValue::Integer(0x12_34_56)
        }]
    );
    assert_eq!(
        valid.features[0]
            .binding
            .decode(2, 3, &WireValue::Integer(0x12_34_56)),
        Some((Property::Color, Some(PropertyValue::Color(rgb))))
    );
    assert_eq!(
        valid.features[0]
            .binding
            .decode(2, 3, &WireValue::Integer(0x01_00_00_00)),
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
            .definition
            .capabilities
            .0
            .contains(&Capability::Color)
    );
}
