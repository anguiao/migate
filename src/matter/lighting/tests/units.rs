use crate::device::TemperatureInterval;
use crate::matter::units::{
    brightness_from_level, kelvin_from_mireds, level_from_brightness, mired_bounds,
    mireds_from_kelvin,
};

#[test]
fn valid_brightness_levels_round_trip_without_losing_precision() {
    for level in 1..=254 {
        assert_eq!(level_from_brightness(brightness_from_level(level)), level);
    }
    assert_eq!(brightness_from_level(1), 1.0);
    assert_eq!(brightness_from_level(254), 100.0);
}

#[test]
fn mired_bounds_and_full_precision_conversion_preserve_physical_ranges() {
    let interval = TemperatureInterval::new(2700, 5700).unwrap();
    let (min, max) = mired_bounds(interval);
    assert_eq!((min, max), (176, 370));
    for mired in min..=max {
        let kelvin = kelvin_from_mireds(mired);
        assert!((2700.0..=5700.0).contains(&kelvin));
    }
    let interval = TemperatureInterval::new(4000, 4001).unwrap();
    assert_eq!(mireds_from_kelvin(interval, 4000), 250);
    assert_eq!(kelvin_from_mireds(250).round() as u32, 4000);
    // 253 mired stays unrounded so the source grid quantizes exactly once:
    // a premature kelvin round would double-round native 2000+2n values.
    let kelvin = kelvin_from_mireds(253);
    assert!((kelvin - 1_000_000.0 / 253.0).abs() < f64::EPSILON);
    assert_eq!(kelvin.round() as u32, 3953);
}
