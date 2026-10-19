//! Valid-model boundary tests: invalid ranges, targets and topologies cannot
//! be constructed at all, so invalid states are unrepresentable by type.

use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, LightColorTarget, LightDimension, LightProfile,
    LightTarget, NumericRange, PhysicalDeviceDescription, RangeError, Rgb, TemperatureInterval,
};

#[test]
fn numeric_ranges_require_a_non_empty_interval_and_a_legal_step() {
    assert_eq!(NumericRange::new(10, 10, 1), Err(RangeError::Empty));
    assert_eq!(NumericRange::new(10, 5, 1), Err(RangeError::Empty));
    assert_eq!(NumericRange::new(1, 100, 0), Err(RangeError::ZeroStep));
    assert_eq!(
        NumericRange::new(1, 100, 100),
        Err(RangeError::StepExceedsSpan)
    );
    let range = NumericRange::new(0, 100, 3).unwrap();
    assert_eq!(range.effective_max(), 99);
    assert_eq!(range.nonzero_bounds(), Some((3, 99)));
    assert_eq!(range.quantize(50.4), 51);
}

#[test]
fn temperature_intervals_require_positive_representable_bounds() {
    for (min, max) in [
        (0, 6500),
        (1, 1_000_001),
        (4500, 4501),
        (1, 6500),
        (10, 6500),
    ] {
        assert!(
            TemperatureInterval::new(min, max).is_err(),
            "{min}..{max} must be rejected"
        );
    }
    let interval = TemperatureInterval::new(2700, 6500).unwrap();
    assert_eq!(interval.min(), 2700);
    assert_eq!(interval.max(), 6500);
}

#[test]
fn temperature_intervals_follow_the_effective_reachable_grid_maximum() {
    let grid = NumericRange::new(2700, 6550, 100).unwrap();
    let interval = TemperatureInterval::from_grid(grid).unwrap();
    assert_eq!(interval.min(), 2700);
    assert_eq!(interval.max(), 6500);
}

#[test]
fn light_targets_are_non_empty_valid_and_decide_their_dimension() {
    assert!(LightTarget::brightness(None, f64::NAN).is_err());
    assert!(LightTarget::brightness(None, 0.0).is_err());
    assert!(LightTarget::brightness(None, 100.5).is_err());
    assert!(LightTarget::temperature(None, 0.0).is_err());

    let brightness = LightTarget::brightness(Some(true), 50.0).unwrap();
    assert_eq!(brightness.dimension(), LightDimension::Brightness);
    assert_eq!(brightness.power(), Some(true));
    assert_eq!(brightness.brightness_percent(), Some(50.0));
    assert_eq!(brightness.color_target(), None);

    let color = LightTarget::temperature(Some(false), 4000.0).unwrap();
    assert_eq!(color.dimension(), LightDimension::Color);
    assert_eq!(
        color.color_target(),
        Some(LightColorTarget::Temperature(4000.0))
    );
    assert_eq!(color.brightness_percent(), None);

    let rgb = LightTarget::rgb(
        None,
        Rgb {
            red: 1,
            green: 2,
            blue: 3,
        },
    );
    assert_eq!(rgb.dimension(), LightDimension::Color);
}

#[test]
fn device_descriptions_require_non_empty_unique_functions() {
    let key = DeviceKey::new("xiaomi", "test");
    assert!(PhysicalDeviceDescription::new(key.clone(), "Test", vec![]).is_err());
    let main = FunctionDescription {
        id: "main".into(),
        label: "Main".into(),
        kind: FunctionKind::Outlet,
    };
    assert!(PhysicalDeviceDescription::new(key.clone(), "Test", vec![main.clone(), main]).is_err());
    let light = FunctionDescription {
        id: "main".into(),
        label: "Light".into(),
        kind: FunctionKind::Light(LightProfile::ColorTemperature(
            TemperatureInterval::new(2700, 6500).unwrap(),
        )),
    };
    let description = PhysicalDeviceDescription::new(key, "Test", vec![light]).unwrap();
    assert_eq!(description.functions().len(), 1);
}
