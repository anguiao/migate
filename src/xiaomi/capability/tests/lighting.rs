use super::key;
use crate::{
    device::{Capability, Category, NumericRange},
    xiaomi::capability::{PropertyBinding, parse_device, parse_device_for_model},
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
        let brightness = lighting.brightness.as_ref().unwrap();
        let temperature = lighting.temperature.as_ref().unwrap();
        assert_eq!(
            brightness.range,
            NumericRange {
                min: 1,
                max: brightness_max,
                step: 1
            }
        );
        assert_eq!(
            temperature.range,
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
                Capability::ColorTemperature(temperature.range)
            ]
        );
        assert_eq!(lighting.color, None);
        assert_eq!(lighting.mode, None);
        assert_eq!(function.power, PropertyBinding { siid: 2, piid: 1 });
        assert_eq!(brightness.property, PropertyBinding { siid: 2, piid: 2 });
        assert_eq!(temperature.property, PropertyBinding { siid: 2, piid: 3 });
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
fn parser_and_cached_bindings_reject_unrepresentable_mired_ranges() {
    let original = description("xiaomi.light.ceil04");
    for (min, max) in [(4500, 4501), (1, 6500), (10, 6500)] {
        let mut instance = original.clone();
        instance["services"][1]["properties"][2]["value-range"] = json!([min, max, 1]);
        assert!(parse_device(key(), &instance).is_err(), "{min}..{max}");
        let parsed = parse_device(key(), &original).unwrap();
        let mut bindings = parsed.functions[0].lighting.clone().unwrap();
        bindings.temperature.as_mut().unwrap().range = NumericRange { min, max, step: 1 };
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
    assert!(
        generic.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .color
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
    let mut optional = original.clone();
    optional["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .retain(|property| {
            !property["type"]
                .as_str()
                .is_some_and(|kind| kind.contains(":color-temperature:"))
        });
    let parsed = parse_device(key(), &optional).unwrap();
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
    let mut empty = original.clone();
    empty["services"][1]["properties"] = json!([]);
    assert!(parse_device(key(), &empty).is_err());
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

fn profile_description(on: bool, brightness: bool, ct: bool, rgb: bool) -> Value {
    let mut instance = description("xiaomi.light.ceil04");
    let properties = instance["services"][1]["properties"]
        .as_array_mut()
        .unwrap();
    properties.retain(|property| {
        let kind = property["type"].as_str().unwrap_or_default();
        (on || !kind.contains(":on:"))
            && (brightness || !kind.contains(":brightness:"))
            && (ct || !kind.contains(":color-temperature:"))
    });
    if rgb {
        properties.push(json!({
            "iid": 100,
            "type": "urn:miot-spec-v2:property:color:0000000E:test-model:1",
            "description": "RGB",
            "format": "uint32",
            "access": ["read", "write", "notify"],
            "unit": "rgb",
            "value-range": [0, 16777215, 1]
        }));
    }
    instance
}

#[test]
fn four_standard_light_profiles_are_parsed_with_the_right_capabilities() {
    use crate::device::{LightMode, LightProfile};
    let ct_range = NumericRange {
        min: 2700,
        max: 5700,
        step: 1,
    };
    for (on, brightness, ct, rgb, profile, capabilities) in [
        (
            true,
            false,
            false,
            false,
            LightProfile::OnOff,
            vec![Capability::OnOff],
        ),
        (
            true,
            true,
            false,
            false,
            LightProfile::Dimmable,
            vec![Capability::OnOff, Capability::Brightness],
        ),
        (
            true,
            true,
            true,
            false,
            LightProfile::ColorTemperature,
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(ct_range),
            ],
        ),
        (
            true,
            true,
            true,
            true,
            LightProfile::ExtendedColor,
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(ct_range),
                Capability::ColorRgb,
            ],
        ),
    ] {
        let parsed = parse_device(key(), &profile_description(on, brightness, ct, rgb))
            .unwrap_or_else(|error| {
                panic!("on={on} brightness={brightness} ct={ct} rgb={rgb}: {error}")
            });
        let function = &parsed.functions[0];
        assert_eq!(
            function.description.capabilities, capabilities,
            "on={on} brightness={brightness} ct={ct} rgb={rgb}"
        );
        assert_eq!(function.description.light_profile().unwrap(), profile);
        assert_eq!(function.power, PropertyBinding { siid: 2, piid: 1 });
        let lighting = function.lighting.as_ref().unwrap();
        assert_eq!(lighting.brightness.is_some(), brightness);
        assert_eq!(lighting.temperature.is_some(), ct);
        assert_eq!(lighting.color.is_some(), rgb);
        assert!(lighting.valid());
        assert_eq!(
            function.description.inferred_color_mode(),
            if profile == LightProfile::ColorTemperature {
                Some(LightMode::Temperature)
            } else {
                None
            }
        );
    }
}

#[test]
fn unsupported_light_profiles_are_rejected_with_a_clear_reason() {
    for (on, brightness, ct, rgb, reason) in [
        (false, false, false, false, "require On/Off"),
        (false, true, false, false, "require On/Off"),
        (false, true, true, false, "require On/Off"),
        (false, false, true, false, "require On/Off"),
        (
            true,
            false,
            true,
            false,
            "unsupported light capability combination",
        ),
        (
            true,
            false,
            false,
            true,
            "unsupported light capability combination",
        ),
        (
            true,
            true,
            false,
            true,
            "unsupported light capability combination",
        ),
    ] {
        let error = parse_device(key(), &profile_description(on, brightness, ct, rgb)).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains(reason),
            "on={on} brightness={brightness} ct={ct} rgb={rgb}: {text}"
        );
    }
}

#[test]
fn present_invalid_optional_properties_are_rejected_and_scene_modes_are_ignored() {
    for (kind, field, value) in [
        (":color:", "unit", json!("kelvin")),
        (":color:", "format", json!("uint16")),
        (":color:", "value-range", json!([0, 65535, 1])),
    ] {
        let mut invalid = profile_description(true, true, true, true);
        let property = invalid["services"][1]["properties"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|property| {
                property["type"]
                    .as_str()
                    .is_some_and(|kind_value| kind_value.contains(kind))
            })
            .unwrap();
        property[field] = value;
        assert!(parse_device(key(), &invalid).is_err(), "{kind} {field}");
    }
    let mut mode_only = profile_description(true, true, true, false);
    mode_only["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "iid": 100,
            "type": "urn:miot-spec-v2:property:mode:00000008:test-model:1",
            "description": "Mode",
            "format": "uint8",
            "access": ["read", "write", "notify"],
            "value-list": [
                {"value": 0, "description": "Warm"},
                {"value": 1, "description": "Color"}
            ]
        }));
    let parsed = parse_device(key(), &mode_only).unwrap();
    assert!(
        parsed.functions[0]
            .lighting
            .as_ref()
            .unwrap()
            .mode
            .is_none()
    );
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
