use super::key;
use crate::{
    device::{Capability, Category, NumericRange},
    xiaomi::capability::{
        BrightnessConversion, PropertyBinding, parse_device, parse_device_for_model,
    },
};
use serde_json::{Value, json};

fn description(model: &str) -> Value {
    // Unmodified public MIoT instance descriptions from miot-spec.org, released versions.
    let fixture = match model {
        "devcea.light.ls2307" => {
            include_str!("../../../../tests/fixtures/miot/devcea-light-ls2307.json")
        }
        "yeelink.light.light3" => {
            include_str!("../../../../tests/fixtures/miot/yeelink-light-light3.json")
        }
        "yeelink.light.ml9" => {
            include_str!("../../../../tests/fixtures/miot/yeelink-light-ml9.json")
        }
        "yeelink.light.spot2" => {
            include_str!("../../../../tests/fixtures/miot/yeelink-light-spot2.json")
        }
        "xiaomi.light.ceil04" => {
            include_str!("../../../../tests/fixtures/miot/xiaomi-light-ceil04.json")
        }
        "xiaomi.light.bar2" => {
            include_str!("../../../../tests/fixtures/miot/xiaomi-light-bar2.json")
        }
        "lemesh.light.wy0c15" => {
            include_str!("../../../../tests/fixtures/miot/lemesh-light-wy0c15.json")
        }
        "synthetic.light.onoff" => {
            include_str!("../../../../tests/fixtures/miot/synthetic-onoff-light.json")
        }
        "synthetic.light.ct" => {
            include_str!("../../../../tests/fixtures/miot/synthetic-ct-light.json")
        }
        "synthetic.light.rgb" => {
            include_str!("../../../../tests/fixtures/miot/synthetic-rgb-light.json")
        }
        "synthetic.light.rgbonly" => {
            include_str!("../../../../tests/fixtures/miot/synthetic-rgbonly-light.json")
        }
        _ => panic!("unknown model"),
    };
    serde_json::from_str(fixture).unwrap()
}

#[test]
fn standard_lights_are_admitted_as_one_main_function() {
    for (model, brightness_max, min_kelvin, max_kelvin) in [
        ("yeelink.light.light3", 65535, 2700, 6500),
        ("yeelink.light.ml9", 100, 3000, 6400),
        ("yeelink.light.spot2", 100, 2700, 6500),
        ("xiaomi.light.ceil04", 100, 2700, 5700),
        ("xiaomi.light.bar2", 100, 2700, 6500),
        ("lemesh.light.wy0c15", 100, 2700, 6500),
    ] {
        let parsed = parse_device(key(), &description(model))
            .unwrap_or_else(|error| panic!("{model}: {error}"));
        assert_eq!(parsed.functions.len(), 1);
        let function = &parsed.functions[0];
        assert_eq!(function.description.id, "main");
        assert_eq!(function.description.category, Category::Light);
        let lighting = function.lighting.as_ref().unwrap();
        assert_eq!(
            lighting.brightness.as_ref().unwrap().range,
            NumericRange {
                min: 1,
                max: brightness_max,
                step: 1
            }
        );
        assert_eq!(
            lighting.temperature.as_ref().unwrap().range,
            NumericRange {
                min: min_kelvin,
                max: max_kelvin,
                step: 1
            }
        );
        assert_eq!(
            function.description.capabilities,
            [
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(lighting.temperature.as_ref().unwrap().range)
            ]
        );
        assert_eq!(lighting.color, None);
        assert_eq!(lighting.mode, None);
        assert_eq!(function.power, Some(PropertyBinding { siid: 2, piid: 1 }));
        assert_eq!(
            lighting.brightness.as_ref().unwrap().property,
            PropertyBinding { siid: 2, piid: 2 }
        );
        assert_eq!(
            lighting.temperature.as_ref().unwrap().property,
            PropertyBinding { siid: 2, piid: 3 }
        );
    }
}

#[test]
fn standard_parser_uses_capabilities_instead_of_a_model_allowlist() {
    let mut instance = description("xiaomi.light.ceil04");
    instance["type"] = json!("urn:miot-spec-v2:device:light:0000A001:unknown-light:7");
    assert!(parse_device_for_model(key(), "unknown.light.v7", &instance).is_ok());
}

#[test]
fn light_groups_are_excluded_even_with_complete_standard_light_capabilities() {
    let instance = description("xiaomi.light.ceil04");
    for (did, model) in [
        ("group.42", "mijia.light.group3"),
        ("group.42", "unknown.light.v7"),
        ("42", "mijia.light.group3"),
    ] {
        assert_eq!(
            parse_device_for_model(
                crate::device::DeviceKey::new("xiaomi", did),
                model,
                &instance
            ),
            Err(crate::xiaomi::capability::CapabilityError::UnsupportedLightGroup)
        );
    }
    assert!(parse_device_for_model(key(), "unknown.light.group3", &instance).is_ok());
}

#[test]
fn parser_keeps_valid_kelvin_ranges_beyond_matter_mired_mapping() {
    let original = description("xiaomi.light.ceil04");
    for (min, max) in [(4500, 4501), (1, 6500), (10, 6500)] {
        let mut instance = original.clone();
        instance["services"][1]["properties"][2]["value-range"] = json!([min, max, 1]);
        let parsed =
            parse_device(key(), &instance).unwrap_or_else(|error| panic!("{min}..{max}: {error}"));
        assert_eq!(
            parsed.functions[0].description.temperature_range(),
            Some(NumericRange { min, max, step: 1 })
        );
        let mut bindings = parsed.functions[0].lighting.clone().unwrap();
        assert!(bindings.valid());
        bindings.temperature.as_mut().unwrap().range = NumericRange {
            min: 0,
            max,
            step: 1,
        };
        assert!(!bindings.valid());
    }
}

#[test]
fn ls2307_combines_exact_rgb_and_mode_binding_without_scene_inference() {
    let instance = description("devcea.light.ls2307");
    let parsed = parse_device_for_model(key(), "devcea.light.ls2307", &instance).unwrap();
    assert_eq!(parsed.functions.len(), 1);
    assert_eq!(parsed.functions[0].description.id, "main");
    let lighting = parsed.functions[0].lighting.as_ref().unwrap();
    assert_eq!(
        lighting.color.as_ref().unwrap().property,
        PropertyBinding { siid: 4, piid: 1 }
    );
    let mode = lighting.mode.as_ref().unwrap();
    assert_eq!(mode.property, PropertyBinding { siid: 4, piid: 2 });
    assert_eq!(mode.temperature_value, 0);
    assert_eq!(mode.rgb_value, 1);
    assert!(
        parsed.functions[0]
            .description
            .capabilities
            .contains(&Capability::ColorRgb)
    );
    let generic = parse_device_for_model(key(), "unknown.light.v1", &instance).unwrap();
    assert_eq!(
        generic.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .color
            .as_ref()
            .unwrap()
            .property,
        PropertyBinding { siid: 4, piid: 1 }
    );
    assert!(
        generic.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .mode
            .is_none()
    );
    for mutation in [
        "missing-mode",
        "wrong-color-access",
        "wrong-mode-enum",
        "wrong-vendor",
    ] {
        let mut invalid = instance.clone();
        match mutation {
            "missing-mode" => {
                invalid["services"][2]["properties"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            "wrong-color-access" => {
                invalid["services"][2]["properties"][0]["access"] = json!(["write"])
            }
            "wrong-mode-enum" => {
                invalid["services"][2]["properties"][1]["value-list"][1]["value"] = json!(9)
            }
            "wrong-vendor" => {
                invalid["type"] = json!("urn:miot-spec-v2:device:light:0000A001:other-model:1")
            }
            _ => unreachable!(),
        }
        assert!(
            parse_device_for_model(key(), "devcea.light.ls2307", &invalid).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn incomplete_invalid_or_ambiguous_light_descriptions_are_rejected() {
    let original = description("xiaomi.light.ceil04");
    for (field, value) in [
        ("format", json!("string")),
        ("access", json!(["read", "write"])),
        ("unit", json!("seconds")),
        ("value-range", json!([1, 100, 0])),
        ("value-range", json!([1, 300, 1])),
        ("value-range", json!([100, 1, 1])),
        ("value-range", json!([1, 100, 0.5])),
        ("value-range", json!([0, 0, 1])),
    ] {
        let mut invalid = original.clone();
        invalid["services"][1]["properties"][1][field] = value;
        assert!(parse_device(key(), &invalid).is_err(), "{field}");
    }
    let mut missing = original.clone();
    missing["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .remove(2);
    let parsed = parse_device(key(), &missing).unwrap();
    assert_eq!(
        parsed.functions[0].description.capabilities,
        [Capability::OnOff, Capability::Brightness]
    );
    assert!(
        parsed.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .temperature
            .is_none()
    );
    let mut sensor_only = original.clone();
    sensor_only["services"].as_array_mut().unwrap().remove(1);
    assert!(parse_device(key(), &sensor_only).is_err());
    let mut ambiguous = original.clone();
    let mut duplicate = ambiguous["services"][1].clone();
    duplicate["iid"] = json!(8);
    ambiguous["services"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(parse_device(key(), &ambiguous).is_err());
}

#[test]
fn brightness_conversion_retains_precision_and_enforces_declared_steps() {
    let parsed = parse_device(key(), &description("yeelink.light.light3")).unwrap();
    let binding = parsed.functions[0]
        .lighting
        .as_ref()
        .unwrap()
        .brightness
        .as_ref()
        .unwrap();
    assert_eq!(binding.brightness_to_raw(1.0).unwrap(), 1);
    assert_eq!(binding.brightness_to_raw(100.0).unwrap(), 65535);
    for raw in [1, 2, 371, 32768, 65534, 65535] {
        assert_eq!(
            binding
                .brightness_to_raw(binding.brightness_from_raw(&json!(raw)).unwrap())
                .unwrap(),
            raw
        );
    }
    assert!(binding.brightness_from_raw(&json!("50")).is_err());
    assert!(binding.brightness_from_raw(&json!(65536)).is_err());
    assert!(binding.brightness_to_raw(f64::NAN).is_err());
    assert!(binding.brightness_to_raw(0.0).is_err());
    let mut stepped = description("xiaomi.light.ceil04");
    stepped["services"][1]["properties"][1]["value-range"] = json!([1, 100, 3]);
    let parsed = parse_device(key(), &stepped).unwrap();
    let binding = parsed.functions[0]
        .lighting
        .as_ref()
        .unwrap()
        .brightness
        .as_ref()
        .unwrap();
    assert!(binding.brightness_from_raw(&json!(2)).is_err());
    assert_eq!(binding.brightness_to_raw(50.0).unwrap(), 49);
    assert_eq!(binding.brightness_to_raw(100.0).unwrap(), 100);
}

#[test]
fn temperature_and_rgb_protocol_values_are_validated_and_quantized() {
    let parsed = parse_device_for_model(
        key(),
        "devcea.light.ls2307",
        &description("devcea.light.ls2307"),
    )
    .unwrap();
    let lighting = parsed.functions[0].lighting.as_ref().unwrap();
    let temperature = lighting.temperature.as_ref().unwrap();
    assert_eq!(temperature.temperature_to_raw(1000).unwrap(), 2700);
    assert_eq!(temperature.temperature_to_raw(10000).unwrap(), 6500);
    assert!(temperature.read_raw(&json!(2700.5)).is_err());
    assert!(temperature.read_raw(&json!(true)).is_err());
    assert!(
        lighting
            .color
            .as_ref()
            .unwrap()
            .read_raw(&json!(16777216))
            .is_err()
    );
}

#[test]
fn zero_based_brightness_preserves_declared_range_and_maps_its_nonzero_grid() {
    for step in [1, 3] {
        let mut instance = description("xiaomi.light.ceil04");
        instance["services"][1]["properties"][1]["value-range"] = json!([0, 100, step]);
        let parsed = parse_device(key(), &instance).unwrap();
        let binding = parsed.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .brightness
            .as_ref()
            .unwrap();
        assert_eq!(binding.range.min, 0);
        assert_eq!(binding.brightness_to_raw(1.0).unwrap(), step);
        assert_eq!(binding.brightness_to_raw(100.0).unwrap(), 100 / step * step);
        assert_eq!(binding.brightness_from_raw(&json!(step)).unwrap(), 1.0);
        assert_eq!(
            binding
                .brightness_from_raw(&json!(100 / step * step))
                .unwrap(),
            100.0
        );
        assert_eq!(binding.brightness_from_raw(&json!(0)).unwrap(), 0.0);
        assert_eq!(binding.brightness_to_raw(0.0).unwrap(), 0);
    }
}

fn synthetic_on(iid: u32) -> Value {
    json!({
        "iid": iid,
        "type": "urn:miot-spec-v2:property:on:00000006:synthetic-v1:1",
        "format": "bool",
        "access": ["read", "write", "notify"]
    })
}

fn synthetic_numeric(
    iid: u32,
    name: &str,
    type_id: &str,
    format: &str,
    unit: &str,
    range: [u32; 3],
) -> Value {
    json!({
        "iid": iid,
        "type": format!("urn:miot-spec-v2:property:{name}:{type_id}:synthetic-v1:1"),
        "format": format,
        "unit": unit,
        "access": ["read", "write", "notify"],
        "value-range": range
    })
}

fn synthetic_color(iid: u32) -> Value {
    synthetic_numeric(iid, "color", "0000000E", "uint32", "rgb", [0, 0xffffff, 1])
}

fn synthetic_subset(on: bool, brightness: bool, temperature: bool, rgb: bool) -> Value {
    synthetic_subset_with_layout(on, brightness, temperature, rgb, false)
}

/// `rgb_in_light` places the standard color property inside the standard light
/// service; otherwise it uses a standard ambient-light service. Both are valid
/// standard expressions and must be admitted from the declared encoding alone.
fn synthetic_subset_with_layout(
    on: bool,
    brightness: bool,
    temperature: bool,
    rgb: bool,
    rgb_in_light: bool,
) -> Value {
    let mut light = Vec::new();
    if on {
        light.push(synthetic_on(1));
    }
    if brightness {
        light.push(synthetic_numeric(
            2,
            "brightness",
            "0000000D",
            "uint8",
            "percentage",
            [1, 100, 1],
        ));
    }
    if temperature {
        light.push(synthetic_numeric(
            3,
            "color-temperature",
            "0000000F",
            "uint16",
            "kelvin",
            [2700, 6500, 1],
        ));
    }
    if rgb && rgb_in_light {
        light.push(synthetic_color(5));
    }
    let mut services = vec![json!({
        "iid": 2,
        "type": "urn:miot-spec-v2:service:light:00007802:synthetic-v1:1",
        "properties": light
    })];
    if rgb && !rgb_in_light {
        services.push(json!({
            "iid": 4,
            "type": "urn:miot-spec-v2:service:ambient-light:0000789D:synthetic-v1:1",
            "properties": [synthetic_color(1)]
        }));
    }
    json!({
        "type": "urn:miot-spec-v2:device:light:0000A001:synthetic-v1:1",
        "services": services
    })
}

#[test]
fn all_fifteen_light_capability_subsets_parse_with_independent_bindings() {
    let mut subsets = 0;
    let mut cases = 0;
    for on in [false, true] {
        for brightness in [false, true] {
            for temperature in [false, true] {
                for rgb in [false, true] {
                    if !(on || brightness || temperature || rgb) {
                        continue;
                    }
                    subsets += 1;
                    let layouts: &[bool] = if rgb { &[false, true] } else { &[false] };
                    for &rgb_in_light in layouts {
                        cases += 1;
                        let instance = synthetic_subset_with_layout(
                            on,
                            brightness,
                            temperature,
                            rgb,
                            rgb_in_light,
                        );
                        let parsed = parse_device(key(), &instance).unwrap_or_else(|error| {
                            panic!(
                                "on={on} brightness={brightness} temperature={temperature} rgb={rgb} rgb_in_light={rgb_in_light}: {error}"
                            )
                        });
                        let mut expected = Vec::new();
                        if on {
                            expected.push(Capability::OnOff);
                        }
                        if brightness {
                            expected.push(Capability::Brightness);
                        }
                        if temperature {
                            expected.push(Capability::ColorTemperature(NumericRange {
                                min: 2700,
                                max: 6500,
                                step: 1,
                            }));
                        }
                        if rgb {
                            expected.push(Capability::ColorRgb);
                        }
                        assert_eq!(parsed.functions[0].description.capabilities, expected);
                        assert_eq!(
                            parsed.functions[0].lighting.is_some(),
                            brightness || temperature || rgb
                        );
                        let json = serde_json::to_value(&parsed.functions[0]).unwrap();
                        assert_eq!(json["power"].is_object(), on);
                        assert_eq!(json["lighting"]["brightness"].is_object(), brightness);
                        assert_eq!(json["lighting"]["temperature"].is_object(), temperature);
                        assert_eq!(json["lighting"]["color"].is_object(), rgb);
                        assert_eq!(
                            json["lighting"]["brightness_conversion"].is_string(),
                            brightness
                        );
                        assert!(json["lighting"]["mode"].is_null());
                        if rgb {
                            let binding = if rgb_in_light {
                                json!({"siid": 2, "piid": 5})
                            } else {
                                json!({"siid": 4, "piid": 1})
                            };
                            assert_eq!(json["lighting"]["color"]["property"], binding);
                        }
                    }
                }
            }
        }
    }
    assert_eq!(subsets, 15);
    assert_eq!(cases, 23);
}

#[test]
fn read_only_fixed_parameters_do_not_count_as_control_capabilities() {
    let mut instance = synthetic_subset(true, true, false, false);
    instance["services"][0]["properties"][1]["access"] = json!(["read", "notify"]);
    let parsed = parse_device(key(), &instance).unwrap();
    assert_eq!(
        parsed.functions[0].description.capabilities,
        [Capability::OnOff]
    );
    let json = serde_json::to_value(&parsed.functions[0]).unwrap();
    assert!(json["lighting"].is_null() || json["lighting"]["brightness"].is_null());

    let mut instance = synthetic_subset(true, false, false, false);
    instance["services"][0]["properties"][0]["access"] = json!(["read"]);
    assert!(parse_device(key(), &instance).is_err());
}

#[test]
fn malformed_controllable_encodings_and_duplicates_are_rejected_explicitly() {
    let mut instance = synthetic_subset(true, false, false, false);
    instance["services"][0]["properties"][0]["format"] = json!("uint8");
    assert!(parse_device(key(), &instance).is_err());

    let mut instance = synthetic_subset(true, true, false, false);
    instance["services"][0]["properties"][1]["access"] = json!(["read", "write"]);
    assert!(parse_device(key(), &instance).is_err());

    for (field, value) in [
        ("unit", json!("seconds")),
        ("value-range", json!([1, 100, 0])),
        ("value-range", json!([0, 0, 1])),
        ("value-range", json!([1, 300, 1])),
        ("format", json!("string")),
    ] {
        let mut instance = synthetic_subset(true, true, false, false);
        instance["services"][0]["properties"][1][field] = value;
        assert!(parse_device(key(), &instance).is_err(), "{field}");
    }

    for rgb_in_light in [false, true] {
        let (service, property) = if rgb_in_light { (0, 3) } else { (1, 0) };
        for (field, value) in [
            ("unit", json!("percentage")),
            ("format", json!("uint16")),
            ("value-range", json!([0, 100, 1])),
            ("value-range", json!([1, 0xffffff, 1])),
        ] {
            let mut instance = synthetic_subset_with_layout(true, true, true, true, rgb_in_light);
            instance["services"][service]["properties"][property][field] = value;
            assert!(
                parse_device(key(), &instance).is_err(),
                "rgb_in_light={rgb_in_light} {field}"
            );
        }
    }

    let mut instance = synthetic_subset(true, true, true, true);
    let mut duplicate = instance["services"][0]["properties"][1].clone();
    duplicate["iid"] = json!(9);
    instance["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(parse_device(key(), &instance).is_err());

    let mut instance = synthetic_subset(true, true, true, true);
    let mut second = instance["services"][1].clone();
    second["iid"] = json!(5);
    instance["services"].as_array_mut().unwrap().push(second);
    assert!(parse_device(key(), &instance).is_err());

    // The standard color property may appear in the light service; a second
    // declaration in the ambient service is still ambiguous.
    let mut instance = synthetic_subset(true, true, true, true);
    instance["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(synthetic_color(5));
    assert!(parse_device(key(), &instance).is_err());
}

#[test]
fn generic_standard_rgb_is_admitted_by_encoding_not_model() {
    let parsed = parse_device_for_model(
        key(),
        "unknown.light.v9",
        &synthetic_subset(false, false, false, true),
    )
    .unwrap();
    assert_eq!(
        parsed.functions[0].description.capabilities,
        [Capability::ColorRgb]
    );
    let json = serde_json::to_value(&parsed.functions[0]).unwrap();
    assert_eq!(
        json["lighting"]["color"]["property"],
        json!({"siid": 4, "piid": 1})
    );

    let mut instance = synthetic_subset(true, false, false, false);
    instance["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(synthetic_color(5));
    let parsed = parse_device(key(), &instance).unwrap();
    assert_eq!(
        parsed.functions[0].description.capabilities,
        [Capability::OnOff, Capability::ColorRgb]
    );
    let json = serde_json::to_value(&parsed.functions[0]).unwrap();
    assert_eq!(
        json["lighting"]["color"]["property"],
        json!({"siid": 2, "piid": 5})
    );
}

#[test]
fn ls2307_private_mode_stays_adapter_specific_while_rgb_is_generic() {
    let instance = description("devcea.light.ls2307");
    let generic = parse_device_for_model(key(), "unknown.light.v1", &instance).unwrap();
    let json = serde_json::to_value(&generic.functions[0]).unwrap();
    assert_eq!(
        json["lighting"]["color"]["property"],
        json!({"siid": 4, "piid": 1})
    );
    assert!(json["lighting"]["mode"].is_null());
    let adapter = parse_device_for_model(key(), "devcea.light.ls2307", &instance).unwrap();
    let json = serde_json::to_value(&adapter.functions[0]).unwrap();
    assert_eq!(
        json["lighting"]["mode"]["property"],
        json!({"siid": 4, "piid": 2})
    );
}

#[test]
fn standard_mode_properties_do_not_establish_a_mode_binding() {
    let mut instance = synthetic_subset(true, false, true, true);
    instance["services"][0]["properties"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "iid": 9,
            "type": "urn:miot-spec-v2:property:mode:00000008:synthetic-v1:1",
            "format": "uint8",
            "access": ["read", "write", "notify"],
            "value-list": [
                {"value": 0, "description": "Warm"},
                {"value": 1, "description": "Color"}
            ]
        }));
    let parsed = parse_device(key(), &instance).unwrap();
    assert_eq!(
        parsed.functions[0].description.capabilities,
        [
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 1,
            }),
            Capability::ColorRgb,
        ]
    );
    let json = serde_json::to_value(&parsed.functions[0]).unwrap();
    assert!(json["lighting"]["mode"].is_null());
}

#[test]
fn light_binding_validation_requires_consistent_brightness_and_dual_color_mode() {
    let parsed = parse_device(key(), &description("xiaomi.light.ceil04")).unwrap();
    let lighting = parsed.functions[0].lighting.clone().unwrap();
    assert!(lighting.valid());
    assert_eq!(
        lighting.brightness_conversion,
        Some(BrightnessConversion::NonzeroLinear)
    );
    let mut without_brightness = lighting;
    without_brightness.brightness = None;
    without_brightness.brightness_conversion = None;
    assert!(without_brightness.valid());
    let mut fabricated_conversion = without_brightness.clone();
    fabricated_conversion.brightness_conversion = Some(BrightnessConversion::NonzeroLinear);
    assert!(!fabricated_conversion.valid());

    let rgb = parse_device_for_model(
        key(),
        "devcea.light.ls2307",
        &description("devcea.light.ls2307"),
    )
    .unwrap();
    let dual = rgb.functions[0].lighting.clone().unwrap();
    assert!(dual.valid());
    let mut without_temperature = dual;
    without_temperature.temperature = None;
    assert!(!without_temperature.valid());
    without_temperature.mode = None;
    assert!(without_temperature.valid());
    without_temperature.brightness_conversion = None;
    assert!(!without_temperature.valid());
}

#[test]
fn valid_kelvin_ranges_are_accepted_even_when_matter_mireds_cannot_represent_them() {
    let original = description("xiaomi.light.ceil04");
    for (min, max) in [(4500, 4501), (1, 6500), (10, 6500)] {
        let mut instance = original.clone();
        instance["services"][1]["properties"][2]["value-range"] = json!([min, max, 1]);
        let parsed =
            parse_device(key(), &instance).unwrap_or_else(|error| panic!("{min}..{max}: {error}"));
        let range = parsed.functions[0].description.temperature_range().unwrap();
        assert_eq!(range, NumericRange { min, max, step: 1 });
        assert!(range.mired_bounds().is_err(), "{min}..{max}");
    }
    for (min, max) in [(0, 6500), (2700, 1_000_001)] {
        let mut instance = original.clone();
        instance["services"][1]["properties"][2]["value-range"] = json!([min, max, 1]);
        assert!(parse_device(key(), &instance).is_err(), "{min}..{max}");
    }
}
