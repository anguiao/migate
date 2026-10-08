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
        assert_eq!(
            lighting.brightness.range,
            NumericRange {
                min: 1,
                max: brightness_max,
                step: 1
            }
        );
        assert_eq!(
            lighting.temperature.range,
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
                Capability::ColorTemperature(lighting.temperature.range)
            ]
        );
        assert_eq!(lighting.color, None);
        assert_eq!(lighting.mode, None);
        assert_eq!(function.power, PropertyBinding { siid: 2, piid: 1 });
        assert_eq!(
            lighting.brightness.property,
            PropertyBinding { siid: 2, piid: 2 }
        );
        assert_eq!(
            lighting.temperature.property,
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
    let mut missing = original.clone();
    missing["services"][1]["properties"]
        .as_array_mut()
        .unwrap()
        .remove(2);
    assert!(parse_device(key(), &missing).is_err());
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
    let binding = &parsed.functions[0].lighting.as_ref().unwrap().brightness;
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
    let binding = &parsed.functions[0].lighting.as_ref().unwrap().brightness;
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
    assert_eq!(lighting.temperature.temperature_to_raw(1000).unwrap(), 2700);
    assert_eq!(
        lighting.temperature.temperature_to_raw(10000).unwrap(),
        6500
    );
    assert!(lighting.temperature.read_raw(&json!(2700.5)).is_err());
    assert!(lighting.temperature.read_raw(&json!(true)).is_err());
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
        let binding = &parsed.functions[0].lighting.as_ref().unwrap().brightness;
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
