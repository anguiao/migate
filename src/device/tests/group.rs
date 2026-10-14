use super::{new_device, outlet};
use crate::device::{
    Availability, Capability, Category, Command, CommandReceiver, DeviceKey, DeviceRegistry,
    Function, FunctionDescription, FunctionKey, Group, GroupError, LightMode, LightPatch,
    NumericRange, PhysicalDevice, PhysicalDeviceDescription, Power, Rgb,
};
use futures_lite::future::{block_on, poll_once};

const CT_FULL: NumericRange = NumericRange {
    min: 2700,
    max: 6500,
    step: 100,
};

fn light_description(key: &str, capabilities: Vec<Capability>) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", key),
        label: key.into(),
        functions: vec![FunctionDescription {
            id: "main".into(),
            label: "Main light".into(),
            category: Category::Light,
            capabilities,
        }],
    }
}

fn ready_light(key: &str, capabilities: Vec<Capability>) -> (PhysicalDevice, CommandReceiver) {
    let (device, receiver) = new_device(light_description(key, capabilities));
    device.set_availability(Availability::Available);
    (device, receiver)
}

fn function_of(device: &PhysicalDevice) -> Function {
    device.function("main").unwrap()
}

fn new_group(key: &str, members: &[&PhysicalDevice]) -> Group {
    Group::new(
        DeviceKey::new("xiaomi", key),
        key,
        members.iter().map(|device| function_of(device)).collect(),
    )
    .unwrap()
}

fn member_key(id: &str) -> FunctionKey {
    FunctionKey::new(DeviceKey::new("xiaomi", id), "main")
}

fn color(red: u8, green: u8, blue: u8) -> Rgb {
    Rgb { red, green, blue }
}

fn observe(function: &Function, patch: LightPatch) {
    function.observe_light(patch).unwrap();
}

fn on_off() -> Vec<Capability> {
    vec![Capability::OnOff]
}

fn dimmable() -> Vec<Capability> {
    vec![Capability::OnOff, Capability::Brightness]
}

fn temperature_light() -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(CT_FULL),
    ]
}

fn rgb_light() -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorRgb,
    ]
}

fn full_light() -> Vec<Capability> {
    vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(CT_FULL),
        Capability::ColorRgb,
    ]
}

#[test]
fn group_capabilities_intersect_every_nonempty_member_combination() {
    let kinds = [
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(CT_FULL),
        Capability::ColorRgb,
    ];
    for mask in 1..16u8 {
        let mut capabilities = Vec::new();
        for (index, kind) in kinds.iter().enumerate() {
            if mask & (1 << index) != 0 {
                capabilities.push(*kind);
            }
        }
        let (first, _first_receiver) = ready_light("first", capabilities.clone());
        let (second, _second_receiver) = ready_light("second", capabilities.clone());
        let group = new_group("group", &[&first, &second]);
        let expected = capabilities
            .iter()
            .map(|capability| match capability {
                Capability::ColorTemperature(range) => Capability::ColorTemperature(NumericRange {
                    min: range.min,
                    max: range.max,
                    step: 1,
                }),
                capability => *capability,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            group.description().capabilities,
            expected,
            "capability mask {mask:#07b}"
        );
    }
}

#[test]
fn group_capabilities_take_the_common_kinds_only() {
    let (temperature, _temperature_receiver) = ready_light("temperature", temperature_light());
    let (rgb, _rgb_receiver) = ready_light("rgb", rgb_light());
    let group = new_group("group", &[&temperature, &rgb]);
    assert_eq!(
        group.description().capabilities,
        vec![Capability::OnOff, Capability::Brightness]
    );
    assert_eq!(group.description().temperature_range(), None);
}

#[test]
fn group_temperature_range_is_the_continuous_common_interval_with_step_one() {
    // The two member grids are offset by one step and never coincide; only the
    // continuous interval intersects.
    let (odd, _odd_receiver) = ready_light(
        "odd",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 200,
            }),
        ],
    );
    let (even, _even_receiver) = ready_light(
        "even",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2800,
                max: 6400,
                step: 200,
            }),
        ],
    );
    let group = new_group("group", &[&odd, &even]);
    assert_eq!(
        group.description().temperature_range(),
        Some(NumericRange {
            min: 2800,
            max: 6400,
            step: 1,
        })
    );

    // A declared max that is not on the member grid contributes its last
    // reachable value as the common bound.
    let (wide, _wide_receiver) = ready_light(
        "wide",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6600,
                step: 100,
            }),
        ],
    );
    let (stepped, _stepped_receiver) = ready_light(
        "stepped",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6550,
                step: 200,
            }),
        ],
    );
    let group = new_group("stepped-group", &[&wide, &stepped]);
    assert_eq!(
        group.description().temperature_range(),
        Some(NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        })
    );
}

#[test]
fn group_temperature_is_dropped_only_when_no_common_interval_exists() {
    let (first, _first_receiver) = ready_light(
        "first",
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 4000,
                step: 100,
            }),
        ],
    );
    let (second, _second_receiver) = ready_light(
        "second",
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(NumericRange {
                min: 4500,
                max: 6500,
                step: 100,
            }),
        ],
    );
    let group = new_group("disjoint", &[&first, &second]);
    assert_eq!(
        group.description().capabilities,
        vec![Capability::OnOff, Capability::Brightness]
    );

    let (third, _third_receiver) = ready_light(
        "third",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 4000,
                step: 100,
            }),
        ],
    );
    let (fourth, _fourth_receiver) = ready_light(
        "fourth",
        vec![
            Capability::OnOff,
            Capability::ColorTemperature(NumericRange {
                min: 4000,
                max: 6500,
                step: 100,
            }),
        ],
    );
    let touching = new_group("touching", &[&third, &fourth]);
    assert_eq!(touching.description().capabilities, vec![Capability::OnOff]);
}

#[test]
fn group_binding_rejects_empty_duplicate_unsupported_and_self_members() {
    let key = DeviceKey::new("xiaomi", "group");
    assert_eq!(
        Group::new(key.clone(), "Group", Vec::new()).err(),
        Some(GroupError::EmptyMembers)
    );

    let (first, _first_receiver) = ready_light("first", dimmable());
    assert_eq!(
        Group::new(
            key.clone(),
            "Group",
            vec![function_of(&first), function_of(&first)]
        )
        .err(),
        Some(GroupError::DuplicateMember(member_key("first")))
    );

    let (outlet_device, _outlet_receiver) = new_device(outlet());
    assert_eq!(
        Group::new(key.clone(), "Group", vec![function_of(&outlet_device)]).err(),
        Some(GroupError::UnsupportedMember(DeviceKey::new(
            "xiaomi",
            "test-outlet"
        )))
    );

    let mut auxiliary = light_description("auxiliary", dimmable());
    auxiliary.functions[0].id = "aux".into();
    let (auxiliary, _auxiliary_receiver) = new_device(auxiliary);
    auxiliary.set_availability(Availability::Available);
    assert_eq!(
        Group::new(key.clone(), "Group", vec![auxiliary.functions()[0].clone()]).err(),
        Some(GroupError::UnsupportedMember(DeviceKey::new(
            "xiaomi",
            "auxiliary"
        )))
    );

    let (self_device, _self_receiver) = ready_light("self", dimmable());
    assert_eq!(
        Group::new(
            DeviceKey::new("xiaomi", "self"),
            "Self",
            vec![function_of(&self_device)]
        )
        .err(),
        Some(GroupError::SelfMember(member_key("self")))
    );
}

#[test]
fn group_uses_live_functions_bound_from_the_registry() {
    let (first, _first_receiver) = ready_light("first", dimmable());
    let (second, _second_receiver) = ready_light("second", dimmable());
    let registry = DeviceRegistry::new();
    registry.insert(first.clone());
    registry.insert(second.clone());
    let group = Group::new(
        DeviceKey::new("xiaomi", "group"),
        "Group",
        [first.key(), second.key()]
            .into_iter()
            .map(|key| registry.get(key).unwrap().function("main").unwrap())
            .collect(),
    )
    .unwrap();

    // Observations through the registry instances are the group's live data.
    for device in [&first, &second] {
        let function = registry
            .get(device.key())
            .unwrap()
            .function("main")
            .unwrap();
        function.observe(true);
    }
    registry
        .get(first.key())
        .unwrap()
        .function("main")
        .unwrap()
        .observe_light(LightPatch {
            brightness_percent: Some(20.0),
            ..LightPatch::default()
        })
        .unwrap();
    registry
        .get(second.key())
        .unwrap()
        .function("main")
        .unwrap()
        .observe_light(LightPatch {
            brightness_percent: Some(40.0),
            ..LightPatch::default()
        })
        .unwrap();
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::On);
    assert_eq!(snapshot.lighting.unwrap().brightness_percent, Some(30.0));
}

#[test]
fn group_power_is_on_when_any_member_is_on_and_off_only_when_all_are_off() {
    let (first, _first_receiver) = ready_light("first", on_off());
    let (second, _second_receiver) = ready_light("second", on_off());
    let group = new_group("group", &[&first, &second]);
    assert_eq!(group.snapshot().power, Power::Unknown);

    function_of(&first).observe(true);
    assert_eq!(group.snapshot().power, Power::On);

    function_of(&first).observe(false);
    assert_eq!(group.snapshot().power, Power::Unknown);
    function_of(&second).observe(false);
    assert_eq!(group.snapshot().power, Power::Off);

    function_of(&second).observe(true);
    assert_eq!(group.snapshot().power, Power::On);
}

#[test]
fn group_reachability_is_true_when_any_member_has_a_usable_path() {
    let (first, _first_receiver) = ready_light("first", dimmable());
    let (second, _second_receiver) = ready_light("second", dimmable());
    let group = new_group("group", &[&first, &second]);
    assert!(group.snapshot().reachable);

    first.set_availability(Availability::Unavailable("offline".into()));
    assert!(group.snapshot().reachable);
    second.set_availability(Availability::Unavailable("offline".into()));
    assert!(!group.snapshot().reachable);
}

#[test]
fn path_loss_invalidates_the_aggregate_and_recovery_does_not_revive_cached_values() {
    let (device, _device_receiver) = ready_light("first", dimmable());
    let group = new_group("group", &[&device]);
    let function = function_of(&device);
    function.observe(true);
    observe(
        &function,
        LightPatch {
            brightness_percent: Some(40.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(group.snapshot().power, Power::On);
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );

    device.set_availability(Availability::Unavailable("offline".into()));
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::Unknown);
    assert_eq!(snapshot.lighting.unwrap().brightness_percent, None);
    assert!(!snapshot.reachable);

    device.set_availability(Availability::Available);
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::Unknown);
    assert_eq!(snapshot.lighting.unwrap().brightness_percent, None);
    // The single-light cache keeps the last values.
    assert_eq!(function.snapshot().power, Power::On);
    assert_eq!(
        function.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );

    // Same-value new observations make the fields valid again.
    function.observe(true);
    observe(
        &function,
        LightPatch {
            brightness_percent: Some(40.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(group.snapshot().power, Power::On);
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );
}

#[test]
fn a_closed_member_stays_unreachable_and_unknown_and_can_never_revive() {
    let (device, receiver) = ready_light("first", on_off());
    let group = new_group("group", &[&device]);
    let function = function_of(&device);
    function.observe(true);
    assert_eq!(group.snapshot().power, Power::On);

    drop(receiver);
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::Unknown);
    assert!(!snapshot.reachable);

    // An old handle cannot restore a closed member.
    function.observe(true);
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::Unknown);
    assert!(!snapshot.reachable);
}

#[test]
fn group_brightness_averages_lit_members_only_when_all_are_complete() {
    let (first, _first_receiver) = ready_light("first", dimmable());
    let (second, _second_receiver) = ready_light("second", dimmable());
    let group = new_group("group", &[&first, &second]);
    function_of(&first).observe(true);
    function_of(&second).observe(true);
    observe(
        &function_of(&first),
        LightPatch {
            brightness_percent: Some(60.0),
            ..LightPatch::default()
        },
    );
    // A lit member without a valid brightness makes the whole group unknown.
    assert_eq!(group.snapshot().lighting.unwrap().brightness_percent, None);
    observe(
        &function_of(&second),
        LightPatch {
            brightness_percent: Some(20.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );

    // An unobserved member power also makes the group brightness unknown.
    let (third, _third_receiver) = ready_light("third", dimmable());
    let (fourth, _fourth_receiver) = ready_light("fourth", dimmable());
    let partial = new_group("partial", &[&third, &fourth]);
    function_of(&third).observe(true);
    observe(
        &function_of(&third),
        LightPatch {
            brightness_percent: Some(50.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(
        partial.snapshot().lighting.unwrap().brightness_percent,
        None
    );
}

#[test]
fn all_off_brightness_keeps_the_last_valid_group_value_in_process() {
    let (first, _first_receiver) = ready_light("first", dimmable());
    let (second, _second_receiver) = ready_light("second", dimmable());
    let group = new_group("group", &[&first, &second]);
    for device in [&first, &second] {
        function_of(device).observe(true);
    }
    observe(
        &function_of(&first),
        LightPatch {
            brightness_percent: Some(30.0),
            ..LightPatch::default()
        },
    );
    observe(
        &function_of(&second),
        LightPatch {
            brightness_percent: Some(50.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );

    for device in [&first, &second] {
        function_of(device).observe(false);
    }
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(40.0)
    );

    // A group without valid history has no all-off brightness.
    let (fresh_one, _fresh_one_receiver) = ready_light("fresh-one", dimmable());
    let (fresh_two, _fresh_two_receiver) = ready_light("fresh-two", dimmable());
    let fresh = new_group("fresh", &[&fresh_one, &fresh_two]);
    function_of(&fresh_one).observe(false);
    function_of(&fresh_two).observe(false);
    assert_eq!(fresh.snapshot().power, Power::Off);
    assert_eq!(fresh.snapshot().lighting.unwrap().brightness_percent, None);
}

#[test]
fn color_representative_prefers_lit_members_then_stable_keys_and_keeps_one_source() {
    let (a, _a_receiver) = ready_light("a", full_light());
    let (b, _b_receiver) = ready_light("b", full_light());
    // Input order is reversed; stable member keys decide.
    let group = new_group("group", &[&b, &a]);
    function_of(&a).observe(false);
    function_of(&b).observe(true);
    observe(
        &function_of(&a),
        LightPatch {
            temperature_kelvin: Some(2700),
            color: Some(color(1, 2, 3)),
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        },
    );
    observe(
        &function_of(&b),
        LightPatch {
            temperature_kelvin: Some(3000),
            color: Some(color(9, 9, 9)),
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    // "a" is dark, so the lit "b" is preferred even though "a" sorts first.
    assert_eq!(snapshot.representative, Some(member_key("b")));
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.mode, Some(LightMode::Temperature));
    assert_eq!(lighting.temperature_kelvin, Some(3000));
    assert_eq!(lighting.color, Some(color(9, 9, 9)));

    // With both lit, the stable key wins regardless of observation order.
    function_of(&a).observe(true);
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("a")));
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.mode, Some(LightMode::Temperature));
    assert_eq!(lighting.temperature_kelvin, Some(2700));
    // The non-active color also comes from the same representative.
    assert_eq!(lighting.color, Some(color(1, 2, 3)));
}

#[test]
fn single_color_kind_members_become_representative_without_mode_observations() {
    let (temperature, _temperature_receiver) = ready_light("temperature", temperature_light());
    let group = new_group("group", &[&temperature]);
    let function = function_of(&temperature);
    function.observe(true);
    // No mode property exists; the fixed mode plus a fresh value is enough.
    observe(
        &function,
        LightPatch {
            temperature_kelvin: Some(3000),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("temperature")));
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.mode, Some(LightMode::Temperature));
    assert_eq!(lighting.temperature_kelvin, Some(3000));

    // Path loss drops the stale value; the fixed mode never needs a report.
    temperature.set_availability(Availability::Unavailable("offline".into()));
    let snapshot = group.snapshot();
    assert_eq!(snapshot.lighting.unwrap().temperature_kelvin, None);
    assert_eq!(snapshot.representative, None);

    // A new value observation restores representative selection.
    temperature.set_availability(Availability::Available);
    observe(
        &function,
        LightPatch {
            temperature_kelvin: Some(3000),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("temperature")));
    assert_eq!(snapshot.lighting.unwrap().temperature_kelvin, Some(3000));
}

#[test]
fn rgb_only_members_use_the_fixed_rgb_mode_without_mode_observations() {
    let (rgb_only, _rgb_receiver) = ready_light("rgb-only", rgb_light());
    let group = new_group("group", &[&rgb_only]);
    let function = function_of(&rgb_only);
    function.observe(true);
    observe(
        &function,
        LightPatch {
            color: Some(color(4, 5, 6)),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("rgb-only")));
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.mode, Some(LightMode::Rgb));
    assert_eq!(lighting.color, Some(color(4, 5, 6)));
}

#[test]
fn dual_color_members_require_an_observed_mode_to_be_representative() {
    let (dual, _dual_receiver) = ready_light("dual", full_light());
    let group = new_group("group", &[&dual]);
    let function = function_of(&dual);
    function.observe(true);
    observe(
        &function,
        LightPatch {
            temperature_kelvin: Some(3000),
            color: Some(color(1, 2, 3)),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.lighting.unwrap().mode, None);
    assert_eq!(snapshot.representative, None);

    observe(
        &function,
        LightPatch {
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("dual")));
    assert_eq!(
        snapshot.lighting.unwrap().mode,
        Some(LightMode::Temperature)
    );
}

#[test]
fn heterogeneous_color_kinds_do_not_project_member_color_fields() {
    let (temperature, _temperature_receiver) = ready_light("temperature", temperature_light());
    let (rgb, _rgb_receiver) = ready_light("rgb", rgb_light());
    let group = new_group("group", &[&temperature, &rgb]);
    assert_eq!(
        group.description().capabilities,
        vec![Capability::OnOff, Capability::Brightness]
    );
    for device in [&temperature, &rgb] {
        function_of(device).observe(true);
    }
    observe(
        &function_of(&temperature),
        LightPatch {
            temperature_kelvin: Some(3000),
            ..LightPatch::default()
        },
    );
    observe(
        &function_of(&rgb),
        LightPatch {
            color: Some(color(1, 2, 3)),
            mode: Some(LightMode::Rgb),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, None);
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.mode, None);
    assert_eq!(lighting.temperature_kelvin, None);
    assert_eq!(lighting.color, None);

    // When the intersection has no dimming kind at all, no lighting state is
    // projected either.
    let (ct_only, _ct_receiver) = ready_light(
        "ct-only",
        vec![Capability::OnOff, Capability::ColorTemperature(CT_FULL)],
    );
    let (rgb_only, _rgb_only_receiver) =
        ready_light("rgb-only", vec![Capability::OnOff, Capability::ColorRgb]);
    let on_off_group = new_group("on-off", &[&ct_only, &rgb_only]);
    assert_eq!(
        on_off_group.description().capabilities,
        vec![Capability::OnOff]
    );
    let snapshot = on_off_group.snapshot();
    assert_eq!(snapshot.lighting, None);
    assert_eq!(snapshot.representative, None);
}

#[test]
fn a_representative_mode_outside_group_capabilities_is_not_replaced_by_a_substitute() {
    let (a, _a_receiver) = ready_light("a", full_light());
    let (b, _b_receiver) = ready_light("b", temperature_light());
    let group = new_group("group", &[&a, &b]);
    assert_eq!(
        group.description().capabilities,
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(NumericRange {
                min: 2700,
                max: 6500,
                step: 1,
            }),
        ]
    );
    function_of(&a).observe(true);
    function_of(&b).observe(false);
    observe(
        &function_of(&a),
        LightPatch {
            mode: Some(LightMode::Rgb),
            color: Some(color(1, 2, 3)),
            temperature_kelvin: Some(3000),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("a")));
    let lighting = snapshot.lighting.unwrap();
    // The representative's RGB mode is not fabricated as temperature, and the
    // group never exposes RGB it does not have.
    assert_eq!(lighting.mode, None);
    assert_eq!(lighting.color, None);
    // The non-active cache still comes from the same representative.
    assert_eq!(lighting.temperature_kelvin, Some(3000));
}

#[test]
fn a_representative_temperature_outside_the_common_range_stays_unknown() {
    let (wide, _wide_receiver) = ready_light("a", temperature_light());
    let (narrow, _narrow_receiver) = ready_light(
        "b",
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(NumericRange {
                min: 3000,
                max: 6000,
                step: 100,
            }),
        ],
    );
    let group = new_group("group", &[&wide, &narrow]);
    assert_eq!(
        group.description().temperature_range(),
        Some(NumericRange {
            min: 3000,
            max: 6000,
            step: 1,
        })
    );
    function_of(&wide).observe(true);
    function_of(&narrow).observe(true);
    observe(
        &function_of(&wide),
        LightPatch {
            temperature_kelvin: Some(2800),
            ..LightPatch::default()
        },
    );
    observe(
        &function_of(&narrow),
        LightPatch {
            temperature_kelvin: Some(3500),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.representative, Some(member_key("a")));
    // Not clamped to the common minimum and not taken from another member.
    assert_eq!(snapshot.lighting.unwrap().temperature_kelvin, None);
}

#[test]
fn partial_reports_preserve_values_while_recovery_restores_only_observed_fields() {
    let (device, _receiver) = ready_light("first", full_light());
    let function = function_of(&device);
    let group = new_group("group", &[&device]);
    observe(
        &function,
        LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            temperature_kelvin: Some(3000),
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        },
    );
    observe(
        &function,
        LightPatch {
            temperature_kelvin: Some(3500),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert_eq!(snapshot.power, Power::On);
    assert_eq!(snapshot.lighting.unwrap().brightness_percent, Some(50.0));

    device.set_availability(Availability::Unavailable("offline".into()));
    assert_eq!(group.snapshot().lighting.unwrap().brightness_percent, None);
    device.set_availability(Availability::Available);
    observe(
        &function,
        LightPatch {
            power: Some(true),
            temperature_kelvin: Some(3500),
            mode: Some(LightMode::Temperature),
            ..LightPatch::default()
        },
    );
    let snapshot = group.snapshot();
    assert!(snapshot.reachable);
    assert_eq!(snapshot.power, Power::On);
    assert_eq!(snapshot.lighting.unwrap().temperature_kelvin, Some(3500));
    assert_eq!(snapshot.lighting.unwrap().brightness_percent, None);
    // The ordinary single-light cache survives the loss of freshness.
    assert_eq!(
        function.snapshot().lighting.unwrap().brightness_percent,
        Some(50.0)
    );

    observe(
        &function,
        LightPatch {
            brightness_percent: Some(50.0),
            ..LightPatch::default()
        },
    );
    assert_eq!(
        group.snapshot().lighting.unwrap().brightness_percent,
        Some(50.0)
    );
}

#[test]
fn an_accepted_power_write_counts_as_a_fresh_group_observation() {
    block_on(async {
        let (device, mut receiver) = ready_light("first", on_off());
        let group = new_group("group", &[&device]);
        let function = function_of(&device);
        let command = function.execute(Command::On);
        futures_lite::pin!(command);
        assert!(poll_once(command.as_mut()).await.is_none());
        let request = receiver.next().await.unwrap();
        request.accept().unwrap();
        command.await.unwrap();
        assert_eq!(group.snapshot().power, Power::On);
    });
}

#[test]
fn same_value_observations_wake_snapshot_subscribers_after_path_loss() {
    block_on(async {
        let (device, _device_receiver) = ready_light("first", dimmable());
        let function = function_of(&device);
        function.observe(true);
        observe(
            &function,
            LightPatch {
                brightness_percent: Some(40.0),
                ..LightPatch::default()
            },
        );
        let mut changes = function.subscribe();

        device.set_availability(Availability::Unavailable("offline".into()));
        assert_eq!(
            changes.changed().await.availability,
            Availability::Unavailable("offline".into())
        );
        device.set_availability(Availability::Available);
        assert_eq!(
            changes.changed().await.availability,
            Availability::Available
        );
        assert_eq!(function.fresh_snapshot().snapshot.power, Power::Unknown);
        assert_eq!(
            function
                .fresh_snapshot()
                .snapshot
                .lighting
                .unwrap()
                .brightness_percent,
            None
        );

        // The cached power value is unchanged; restoring validity alone must
        // still wake snapshot subscribers.
        let mut restored = Box::pin(changes.changed());
        assert!(poll_once(restored.as_mut()).await.is_none());
        function.observe(true);
        assert_eq!(restored.await.power, Power::On);

        // The same holds for a same-value light report.
        let mut restored = Box::pin(changes.changed());
        assert!(poll_once(restored.as_mut()).await.is_none());
        function
            .observe_light(LightPatch {
                brightness_percent: Some(40.0),
                ..LightPatch::default()
            })
            .unwrap();
        let snapshot = restored.await;
        assert_eq!(snapshot.lighting.unwrap().brightness_percent, Some(40.0));
    });
}

#[test]
fn closing_a_function_wakes_snapshot_subscribers() {
    block_on(async {
        let (device, receiver) = ready_light("first", on_off());
        let function = function_of(&device);
        let group = new_group("group", &[&device]);
        function.observe(true);
        let mut changes = function.subscribe();
        let mut closed = Box::pin(changes.changed());
        assert!(poll_once(closed.as_mut()).await.is_none());

        drop(receiver);
        let snapshot = closed.await;
        assert_eq!(snapshot.power, Power::On);
        assert!(!function.fresh_snapshot().reachable);
        assert_eq!(group.snapshot().power, Power::Unknown);
    });
}
