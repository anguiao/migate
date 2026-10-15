use crate::device::{Capability, DeviceKey, LightProfile, NumericRange, standard_light_profile};
use crate::xiaomi::capability::{
    ParsedDevice, effective_temperature_range, parse_device_for_model, parse_group,
    plan_rgb_repair, repaired_capabilities,
};
use serde_json::{Value, json};

fn fixture(path: &str) -> Value {
    serde_json::from_str(path).unwrap()
}

fn group3() -> Value {
    fixture(include_str!(
        "../../../../tests/fixtures/miot/group3-instance.json"
    ))
}

fn ls2307() -> Value {
    fixture(include_str!(
        "../../../../tests/fixtures/miot/devcea-light-ls2307.json"
    ))
}

fn light3() -> Value {
    fixture(include_str!(
        "../../../../tests/fixtures/miot/yeelink-light-light3.json"
    ))
}

fn parse_group3() -> ParsedDevice {
    parse_group(
        DeviceKey::new("xiaomi", "group-1"),
        "mijia.light.group3",
        &group3(),
    )
    .unwrap()
}

fn parse_model(did: &str, model: &str, instance: &Value) -> ParsedDevice {
    parse_device_for_model(DeviceKey::new("xiaomi", did), model, instance).unwrap()
}

fn member_temperature(device: &ParsedDevice) -> NumericRange {
    device.functions[0]
        .lighting
        .as_ref()
        .unwrap()
        .temperature
        .as_ref()
        .unwrap()
        .range
}

#[test]
fn ls2307_members_extend_group_profile_and_narrow_temperature() {
    let group = parse_group3();
    let a = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    let b = parse_model("member-b", "devcea.light.ls2307", &ls2307());
    let urn = ls2307()["type"].as_str().unwrap().to_string();
    let plan = plan_rgb_repair(
        &group,
        [
            ("member-a", "devcea.light.ls2307", urn.as_str(), &a),
            ("member-b", "devcea.light.ls2307", urn.as_str(), &b),
        ],
    )
    .unwrap()
    .expect("repair applies");
    assert_eq!(
        plan.members,
        [
            DeviceKey::new("xiaomi", "member-a"),
            DeviceKey::new("xiaomi", "member-b"),
        ]
    );
    let logical = effective_temperature_range(
        member_temperature(&group),
        &[member_temperature(&a), member_temperature(&b)],
    );
    assert_eq!(
        logical,
        NumericRange {
            min: 2700,
            max: 6500,
            step: 1
        }
    );
    let capabilities = repaired_capabilities(&group.functions[0], logical);
    assert!(capabilities.contains(&Capability::ColorRgb));
    assert!(capabilities.contains(&Capability::ColorTemperature(logical)));
    assert_eq!(
        standard_light_profile(&capabilities),
        Ok(LightProfile::ExtendedColor)
    );
}

#[test]
fn repair_requires_every_member_to_be_the_concrete_ls2307_adapter() {
    let group = parse_group3();
    let ls = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    let urn = ls2307()["type"].as_str().unwrap().to_string();
    assert!(
        plan_rgb_repair(
            &group,
            std::iter::empty::<(&str, &str, &str, &ParsedDevice)>(),
        )
        .is_err(),
        "a group without members cannot be repaired"
    );
    let obct = parse_model("member-b", "yeelink.light.light3", &light3());
    assert!(
        plan_rgb_repair(
            &group,
            [(
                "member-b",
                "yeelink.light.light3",
                "urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1",
                &obct,
            )],
        )
        .is_err()
    );
    assert!(
        plan_rgb_repair(
            &group,
            [("member-a", "not.light.ls2307", urn.as_str(), &ls)],
        )
        .is_err(),
        "a non-ls2307 model must not be planned"
    );
    assert!(
        plan_rgb_repair(
            &group,
            [(
                "member-a",
                "devcea.light.ls2307",
                "urn:miot-spec-v2:device:light:0000A001:other:1",
                &ls
            )],
        )
        .is_err(),
        "an ls2307 model with a different instance type must not be planned"
    );
}

#[test]
fn native_rgb_is_not_repaired_and_scene_mode_is_not_rgb() {
    // A native instance that already exposes real RGB is used as-is.
    let native = parse_group(
        DeviceKey::new("xiaomi", "group-1"),
        "devcea.light.ls2307",
        &ls2307(),
    )
    .unwrap();
    assert!(
        matches!(
            plan_rgb_repair(
                &native,
                std::iter::empty::<(&str, &str, &str, &ParsedDevice)>(),
            ),
            Ok(None)
        ),
        "native RGB needs no repair"
    );
    // group3 carries a scene mode property; it is not an RGB binding, so
    // repair planning still applies.
    let group = parse_group3();
    let ls = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    let urn = ls2307()["type"].as_str().unwrap().to_string();
    assert!(
        plan_rgb_repair(
            &group,
            [("member-a", "devcea.light.ls2307", urn.as_str(), &ls)]
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn effective_range_falls_back_and_keeps_the_genuine_native_grid() {
    let native = NumericRange {
        min: 1700,
        max: 7000,
        step: 100,
    };
    // A member bound that is off the native grid still yields a continuous
    // logical interval bounded by the explicit member range.
    assert_eq!(
        effective_temperature_range(
            native,
            &[NumericRange {
                min: 2750,
                max: 6450,
                step: 1
            }]
        ),
        NumericRange {
            min: 2750,
            max: 6450,
            step: 1
        }
    );
    // A disjoint member range keeps the native declaration, grid included.
    assert_eq!(
        effective_temperature_range(
            native,
            &[NumericRange {
                min: 1000,
                max: 1600,
                step: 1
            }]
        ),
        native
    );
    // The genuine native binding stays on its own grid after parsing.
    let mut mutated = group3();
    mutated["services"][1]["properties"][2]["value-range"] = json!([1700, 7000, 100]);
    let group = parse_group(
        DeviceKey::new("xiaomi", "group-1"),
        "mijia.light.group3",
        &mutated,
    )
    .unwrap();
    assert_eq!(member_temperature(&group), native);
}
