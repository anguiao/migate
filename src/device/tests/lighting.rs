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
    let (min, max) = range.mired_bounds();
    assert_eq!((min, max), (176, 370));
    for mired in min..=max {
        let kelvin = range.kelvin_from_mireds(mired).unwrap();
        assert!(range.contains(kelvin));
    }
    assert!(range.kelvin_from_mireds(0).is_err());
}
