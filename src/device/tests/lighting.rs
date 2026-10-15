use super::{new_device, outlet};
use crate::device::{
    Capability, Category, LightMode, LightPatch, NumericRange, Rgb, brightness_from_matter_level,
    brightness_to_matter_level,
};

fn light(rgb: bool) -> crate::device::PhysicalDeviceDescription {
    let mut description = outlet();
    let function = &mut description.functions[0];
    function.category = Category::Light;
    function.capabilities.extend([
        Capability::Brightness,
        Capability::ColorTemperature(NumericRange {
            min: 2700,
            max: 6500,
            step: 1,
        }),
    ]);
    if rgb {
        function.capabilities.push(Capability::ColorRgb);
    }
    description
}

fn capability_light(capabilities: Vec<Capability>) -> crate::device::PhysicalDeviceDescription {
    let mut description = outlet();
    let function = &mut description.functions[0];
    function.category = Category::Light;
    function.capabilities = capabilities;
    description
}

fn temperature_range() -> NumericRange {
    NumericRange {
        min: 2700,
        max: 6500,
        step: 1,
    }
}

#[test]
fn four_standard_light_profiles_are_valid_and_infer_only_temperature_mode() {
    use crate::device::LightProfile;
    let ct = temperature_range();
    for (capabilities, profile) in [
        (vec![Capability::OnOff], LightProfile::OnOff),
        (
            vec![Capability::OnOff, Capability::Brightness],
            LightProfile::Dimmable,
        ),
        (
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(ct),
            ],
            LightProfile::ColorTemperature,
        ),
        (
            vec![
                Capability::OnOff,
                Capability::Brightness,
                Capability::ColorTemperature(ct),
                Capability::ColorRgb,
            ],
            LightProfile::ExtendedColor,
        ),
    ] {
        let (device, _) = new_device(capability_light(capabilities.clone()));
        let function = &device.functions()[0];
        assert!(
            function.description().valid_capabilities(),
            "{capabilities:?}"
        );
        assert_eq!(
            function.description().light_profile().unwrap(),
            profile,
            "{capabilities:?}"
        );
        let expected =
            (profile == LightProfile::ColorTemperature).then_some(LightMode::Temperature);
        assert_eq!(
            function.description().inferred_color_mode(),
            expected,
            "{capabilities:?}"
        );
        let lighting = function.snapshot().lighting.expect("light state");
        assert_eq!(lighting.brightness_percent, None, "{capabilities:?}");
        assert_eq!(lighting.temperature_kelvin, None, "{capabilities:?}");
        assert_eq!(lighting.color, None, "{capabilities:?}");
        assert_eq!(lighting.mode, expected, "{capabilities:?}");
    }
}

#[test]
fn unsupported_light_capability_sets_are_invalid() {
    let ct = temperature_range();
    for capabilities in [
        vec![Capability::Brightness],
        vec![Capability::ColorTemperature(ct)],
        vec![Capability::ColorRgb],
        vec![Capability::Brightness, Capability::ColorTemperature(ct)],
        vec![Capability::Brightness, Capability::ColorRgb],
        vec![Capability::OnOff, Capability::ColorTemperature(ct)],
        vec![Capability::OnOff, Capability::ColorRgb],
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorRgb,
        ],
        vec![
            Capability::OnOff,
            Capability::Brightness,
            Capability::ColorTemperature(ct),
            Capability::OnOff,
        ],
    ] {
        let description = capability_light(capabilities.clone());
        assert!(
            !description.functions[0].valid_capabilities(),
            "{capabilities:?}"
        );
        assert!(
            description.functions[0].light_profile().is_err(),
            "{capabilities:?}"
        );
    }
}

#[test]
fn losing_all_paths_clears_light_state_and_keeps_single_color_mode() {
    let (device, _) = new_device(capability_light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(temperature_range()),
    ]));
    let function = &device.functions()[0];
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(50.0),
            temperature_kelvin: Some(3000),
            ..Default::default()
        })
        .unwrap();
    device.set_availability(crate::device::Availability::Unavailable(
        "paths lost".into(),
    ));
    let snapshot = function.snapshot();
    assert_eq!(snapshot.power, crate::device::Power::Unknown);
    let lighting = snapshot.lighting.unwrap();
    assert_eq!(lighting.brightness_percent, None);
    assert_eq!(lighting.temperature_kelvin, None);
    assert_eq!(lighting.color, None);
    assert_eq!(lighting.mode, Some(LightMode::Temperature));

    let (device, _) = new_device(capability_light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(temperature_range()),
        Capability::ColorRgb,
    ]));
    let function = &device.functions()[0];
    function
        .observe_light(LightPatch {
            mode: Some(LightMode::Rgb),
            color: Some(Rgb {
                red: 1,
                green: 2,
                blue: 3,
            }),
            ..Default::default()
        })
        .unwrap();
    device.set_availability(crate::device::Availability::Unavailable(
        "paths lost".into(),
    ));
    assert_eq!(function.snapshot().lighting.unwrap().mode, None);
}

#[test]
fn outlet_state_is_unchanged_by_light_state_reset() {
    let (device, _) = new_device(outlet());
    device.functions()[0].observe(true);
    device.set_availability(crate::device::Availability::Unavailable(
        "paths lost".into(),
    ));
    assert_eq!(
        device.functions()[0].snapshot().power,
        crate::device::Power::On
    );
}

#[test]
fn reports_for_absent_capabilities_are_unsupported() {
    let rgb = Rgb {
        red: 1,
        green: 2,
        blue: 3,
    };
    let (device, _) = new_device(capability_light(vec![Capability::OnOff]));
    let function = &device.functions()[0];
    assert!(
        function
            .observe_light(LightPatch {
                brightness_percent: Some(50.0),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        function
            .observe_light(LightPatch {
                color: Some(rgb),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .is_err()
    );

    let (device, _) = new_device(capability_light(vec![
        Capability::OnOff,
        Capability::Brightness,
    ]));
    let function = &device.functions()[0];
    assert!(
        function
            .observe_light(LightPatch {
                temperature_kelvin: Some(3000),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Temperature),
                ..Default::default()
            })
            .is_err()
    );

    let (device, _) = new_device(capability_light(vec![
        Capability::OnOff,
        Capability::Brightness,
        Capability::ColorTemperature(temperature_range()),
    ]));
    let function = &device.functions()[0];
    assert!(
        function
            .observe_light(LightPatch {
                color: Some(rgb),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        function
            .observe_light(LightPatch {
                mode: Some(LightMode::Rgb),
                ..Default::default()
            })
            .is_err()
    );
}

#[test]
fn lighting_starts_unknown_except_for_capability_determined_temperature_mode() {
    for rgb in [false, true] {
        let (device, _) = new_device(light(rgb));
        let lighting = device.functions()[0].snapshot().lighting.unwrap();
        assert_eq!(lighting.brightness_percent, None);
        assert_eq!(lighting.temperature_kelvin, None);
        assert_eq!(lighting.color, None);
        assert_eq!(
            lighting.mode,
            if rgb {
                None
            } else {
                Some(LightMode::Temperature)
            }
        );
    }
}

#[test]
fn one_light_report_updates_power_and_values_with_one_revision_and_keeps_mode_explicit() {
    let (device, _) = new_device(light(true));
    let function = &device.functions()[0];
    function
        .observe_light(LightPatch {
            power: Some(true),
            brightness_percent: Some(52.125),
            temperature_kelvin: Some(3100),
            color: Some(Rgb {
                red: 255,
                green: 0,
                blue: 0,
            }),
            mode: Some(LightMode::Temperature),
        })
        .unwrap();
    assert_eq!(function.snapshot().revision, 1);
    let patch = LightPatch {
        color: Some(Rgb {
            red: 0,
            green: 0,
            blue: 255,
        }),
        ..LightPatch::default()
    };
    function.observe_light(patch).unwrap();
    assert_eq!(
        function.snapshot().lighting.unwrap().mode,
        Some(LightMode::Temperature)
    );
    assert_eq!(function.snapshot().revision, 2);
    function.observe_light(patch).unwrap();
    assert_eq!(function.snapshot().revision, 2);
    let invalid = LightPatch {
        power: Some(false),
        brightness_percent: Some(f64::NAN),
        ..LightPatch::default()
    };
    assert!(function.observe_light(invalid).is_err());
    assert_eq!(function.snapshot().power, crate::device::Power::On);
}

#[test]
fn brightness_and_temperature_matter_conversion_preserve_physical_ranges() {
    for level in 1..=254 {
        assert_eq!(
            brightness_to_matter_level(brightness_from_matter_level(level).unwrap()).unwrap(),
            level
        );
    }
    assert_eq!(brightness_from_matter_level(1).unwrap(), 1.0);
    assert_eq!(brightness_from_matter_level(254).unwrap(), 100.0);
    assert!(brightness_from_matter_level(0).is_err());
    assert!(brightness_to_matter_level(f64::INFINITY).is_err());
    let range = NumericRange {
        min: 2700,
        max: 5700,
        step: 100,
    };
    let (min, max) = range.mired_bounds().unwrap();
    assert_eq!((min, max), (176, 370));
    for mired in min..=max {
        let kelvin = range.kelvin_from_mireds(mired).unwrap();
        assert!(range.contains(kelvin));
    }
    assert!(range.kelvin_from_mireds(0).is_err());
}

#[test]
fn matter_temperature_rejects_unrepresentable_ranges_without_panicking() {
    for range in [
        NumericRange {
            min: 4500,
            max: 4501,
            step: 1,
        },
        NumericRange {
            min: 10,
            max: 6500,
            step: 1,
        },
        NumericRange {
            min: 1,
            max: 6500,
            step: 1,
        },
    ] {
        assert!(range.mireds_from_kelvin(range.min).is_err());
        assert!(range.kelvin_from_mireds(250).is_err());
        let mut description = light(false);
        description.functions[0].capabilities[2] = Capability::ColorTemperature(range);
        assert!(!description.functions[0].valid_capabilities());
    }
    let range = NumericRange {
        min: 4000,
        max: 4001,
        step: 1,
    };
    assert_eq!(range.mireds_from_kelvin(4000).unwrap(), 250);
    assert_eq!(range.kelvin_from_mireds(250).unwrap(), 4000);
}
