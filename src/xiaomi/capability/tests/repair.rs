use crate::device::{DeviceKey, LightProfile, LightProfileKind, NumericRange, TemperatureInterval};
use crate::xiaomi::capability::{
    NativeDescription, ParsedDevice, effective_temperature_range, parse_device_for_model,
    parse_group, plan_rgb_repair, repaired_profile,
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

fn parse_model(did: &str, model: &str, instance: &Value) -> NativeDescription {
    NativeDescription::new(
        model.into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(DeviceKey::new("xiaomi", did), model, instance).unwrap(),
    )
    .unwrap()
}

fn member_temperature(device: &ParsedDevice) -> TemperatureInterval {
    device.functions()[0]
        .lighting()
        .unwrap()
        .temperature()
        .unwrap()
        .interval()
}

#[test]
fn ls2307_members_extend_group_profile_and_narrow_temperature() {
    let group = parse_group3();
    let a = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    let b = parse_model("member-b", "devcea.light.ls2307", &ls2307());
    let plan = plan_rgb_repair(&group, [&a, &b])
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
        &[
            member_temperature(a.device()),
            member_temperature(b.device()),
        ],
    );
    assert_eq!(logical, TemperatureInterval::new(2700, 6500).unwrap());
    let profile = repaired_profile(logical);
    assert!(profile.has_rgb());
    assert_eq!(profile.temperature_interval(), Some(logical));
    assert_eq!(profile.kind(), LightProfileKind::ExtendedColor);
}

#[test]
fn repair_requires_every_member_to_be_the_concrete_ls2307_adapter() {
    let group = parse_group3();
    let ls = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    assert!(
        plan_rgb_repair(&group, []).is_err(),
        "a group without members cannot be repaired"
    );
    let obct = parse_model("member-b", "yeelink.light.light3", &light3());
    assert!(plan_rgb_repair(&group, [&ls, &obct]).is_err());
    assert!(
        NativeDescription::new(
            "not.light.ls2307".into(),
            ls.instance_type().into(),
            ls.device().clone(),
        )
        .is_err(),
        "an ls2307 control strategy requires the matching model"
    );
    assert!(
        NativeDescription::new(
            "devcea.light.ls2307".into(),
            "urn:miot-spec-v2:device:light:0000A001:other:1".into(),
            ls.device().clone()
        )
        .is_err(),
        "an incompatible native description is rejected before planning"
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
        matches!(plan_rgb_repair(&native, [],), Ok(None)),
        "native RGB needs no repair"
    );
    // group3 carries a scene mode property; it is not an RGB binding, so
    // repair planning still applies.
    let group = parse_group3();
    let ls = parse_model("member-a", "devcea.light.ls2307", &ls2307());
    assert!(plan_rgb_repair(&group, [&ls]).unwrap().is_some());
}

#[test]
fn effective_range_falls_back_and_keeps_the_genuine_native_grid() {
    let native_grid = NumericRange::new(1700, 7000, 100).unwrap();
    let native = TemperatureInterval::from_grid(native_grid).unwrap();
    // A member bound that is off the native grid still yields a continuous
    // logical interval bounded by the explicit member interval.
    assert_eq!(
        effective_temperature_range(native, &[TemperatureInterval::new(2750, 6450).unwrap()]),
        TemperatureInterval::new(2750, 6450).unwrap()
    );
    // A disjoint member range keeps the native declaration.
    assert_eq!(
        effective_temperature_range(native, &[TemperatureInterval::new(1000, 1600).unwrap()]),
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
    assert_eq!(
        group.functions()[0]
            .lighting()
            .unwrap()
            .temperature()
            .unwrap()
            .range(),
        native_grid
    );
}

#[test]
fn narrowed_profile_preserves_native_rgb_but_replaces_only_the_interval() {
    let native = LightProfile::ExtendedColor(TemperatureInterval::new(2700, 6500).unwrap());
    let narrowed = crate::xiaomi::capability::narrowed_profile(
        native,
        TemperatureInterval::new(3000, 6000).unwrap(),
    );
    assert_eq!(
        narrowed,
        LightProfile::ExtendedColor(TemperatureInterval::new(3000, 6000).unwrap())
    );
}

#[test]
fn repair_never_selects_an_arbitrary_member_light() {
    use crate::xiaomi::capability::{LightBindings, ParsedFunction, PropertyBinding};
    let group = parse_group3();
    let original = parse_model("member", "devcea.light.ls2307", &ls2307());
    let mut functions = original.device().functions().to_vec();
    functions.push(ParsedFunction::light(
        "light:99",
        "Second light",
        PropertyBinding { siid: 99, piid: 1 },
        LightBindings::OnOff,
    ));
    let member = NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        ParsedDevice::new(original.device().key().clone(), functions, None).unwrap(),
    )
    .unwrap();
    let reason = plan_rgb_repair(&group, [&member]).unwrap_err();
    assert!(reason.contains("ambiguous"), "{reason}");
}
