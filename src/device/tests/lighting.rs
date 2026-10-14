use super::{Availability, Power, new_device, outlet};
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
fn lighting_starts_and_recovers_unknown_except_for_capability_determined_mode() {
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
        device.set_availability(Availability::Available);
        let function = &device.functions()[0];
        function
            .observe_light(LightPatch {
                power: Some(true),
                brightness_percent: Some(60.0),
                temperature_kelvin: Some(3100),
                color: rgb.then(|| Rgb::from_packed(0xff0000).unwrap()),
                mode: Some(LightMode::Temperature),
            })
            .unwrap();
        device.set_availability(Availability::Unavailable("offline".into()));
        assert_eq!(function.snapshot().power, Power::Unknown);
        assert_eq!(function.snapshot().lighting, Some(lighting));
        device.set_availability(Availability::Available);
        assert_eq!(function.snapshot().power, Power::Unknown);
        assert_eq!(function.snapshot().lighting, Some(lighting));
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
    let before = function.snapshot();
    for invalid in [
        LightPatch {
            power: Some(false),
            brightness_percent: Some(f64::NAN),
            ..LightPatch::default()
        },
        LightPatch {
            temperature_kelvin: Some(1000),
            ..LightPatch::default()
        },
    ] {
        assert!(function.observe_light(invalid).is_err());
        assert_eq!(function.snapshot(), before);
    }
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
fn matter_temperature_mapping_rejects_unrepresentable_ranges_without_panicking() {
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
    }
    let range = NumericRange {
        min: 4000,
        max: 4001,
        step: 1,
    };
    assert_eq!(range.mireds_from_kelvin(4000).unwrap(), 250);
    assert_eq!(range.kelvin_from_mireds(250).unwrap(), 4000);
}
