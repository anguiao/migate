use super::{new_device, outlet};
use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, LightMode, LightPatch, LightProfile,
    PhysicalDeviceDescription, Rgb, TemperatureInterval,
};

fn interval() -> TemperatureInterval {
    TemperatureInterval::new(2700, 6500).unwrap()
}

fn description(kind: FunctionKind) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", "test-outlet"),
        "Test Outlet",
        vec![FunctionDescription {
            id: "main".into(),
            label: "Main power".into(),
            kind,
        }],
    )
    .unwrap()
}

fn light(rgb: bool) -> PhysicalDeviceDescription {
    let profile = if rgb {
        LightProfile::ExtendedColor(interval())
    } else {
        LightProfile::ColorTemperature(interval())
    };
    description(FunctionKind::Light(profile))
}

#[test]
fn four_standard_light_profiles_are_valid_and_infer_only_temperature_mode() {
    for profile in [
        LightProfile::OnOff,
        LightProfile::Dimmable,
        LightProfile::ColorTemperature(interval()),
        LightProfile::ExtendedColor(interval()),
    ] {
        let (device, _) = new_device(description(FunctionKind::Light(profile)));
        let function = &device.functions()[0];
        assert_eq!(function.description().kind, FunctionKind::Light(profile));
        let expected = if matches!(profile, LightProfile::ColorTemperature(_)) {
            Some(LightMode::Temperature)
        } else {
            None
        };
        assert_eq!(function.description().inferred_color_mode(), expected);
        let lighting = function.snapshot().lighting().expect("light state");
        assert_eq!(lighting.brightness_percent, None);
        assert_eq!(lighting.temperature_kelvin, None);
        assert_eq!(lighting.color, None);
        assert_eq!(lighting.mode, expected);
    }
}

#[test]
fn losing_all_paths_clears_light_state_and_keeps_single_color_mode() {
    let (device, _) = new_device(description(FunctionKind::Light(
        LightProfile::ColorTemperature(interval()),
    )));
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
    assert_eq!(snapshot.power(), Some(crate::device::Power::Unknown));
    let lighting = snapshot.lighting().unwrap();
    assert_eq!(lighting.brightness_percent, None);
    assert_eq!(lighting.temperature_kelvin, None);
    assert_eq!(lighting.color, None);
    assert_eq!(lighting.mode, Some(LightMode::Temperature));

    let (device, _) = new_device(description(FunctionKind::Light(
        LightProfile::ExtendedColor(interval()),
    )));
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
    assert_eq!(function.snapshot().lighting().unwrap().mode, None);
}

#[test]
fn outlet_state_is_unchanged_by_light_state_reset() {
    let (device, _) = new_device(outlet());
    device.functions()[0].observe(true);
    device.set_availability(crate::device::Availability::Unavailable(
        "paths lost".into(),
    ));
    assert_eq!(
        device.functions()[0].snapshot().power(),
        Some(crate::device::Power::On)
    );
}

#[test]
fn reports_for_absent_capabilities_are_unsupported() {
    let rgb = Rgb {
        red: 1,
        green: 2,
        blue: 3,
    };
    let (device, _) = new_device(description(FunctionKind::Light(LightProfile::OnOff)));
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

    let (device, _) = new_device(description(FunctionKind::Light(LightProfile::Dimmable)));
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

    let (device, _) = new_device(description(FunctionKind::Light(
        LightProfile::ColorTemperature(interval()),
    )));
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
        let lighting = device.functions()[0].snapshot().lighting().unwrap();
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
    assert_eq!(function.snapshot().revision(), 1);
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
        function.snapshot().lighting().unwrap().mode,
        Some(LightMode::Temperature)
    );
    assert_eq!(function.snapshot().revision(), 2);
    function.observe_light(patch).unwrap();
    assert_eq!(function.snapshot().revision(), 2);
    let invalid = LightPatch {
        power: Some(false),
        brightness_percent: Some(f64::NAN),
        ..LightPatch::default()
    };
    assert!(function.observe_light(invalid).is_err());
    assert_eq!(function.snapshot().power(), Some(crate::device::Power::On));
}
