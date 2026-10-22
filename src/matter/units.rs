//! Matter representation boundary for light level and color-temperature units.
//!
//! The effective device model keeps logical kelvin intervals and native
//! quantization grids. Converting between level percent and Matter's 1..254
//! scale, and between kelvin and the Matter mired scale, is a representation
//! concern owned here. Mired to kelvin keeps full precision so the native grid
//! is quantized exactly once, at the Xiaomi send boundary.

use crate::device::{CommandError, MeasurementRange, TemperatureInterval, mired_span};

/// Matter nonzero level to device brightness percent.
pub(crate) fn brightness_from_level(level: u8) -> Result<f64, CommandError> {
    if !(1..=254).contains(&level) {
        return Err(CommandError::Rejected(
            "brightness level is outside 1..254".into(),
        ));
    }
    Ok(1.0 + (level as f64 - 1.0) * 99.0 / 253.0)
}

/// Device brightness percent to the Matter nonzero level scale.
pub(crate) fn level_from_brightness(percent: f64) -> Result<u8, CommandError> {
    if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
        return Err(CommandError::Rejected(
            "brightness is outside 1..100 percent".into(),
        ));
    }
    Ok((1.0 + (percent - 1.0) * 253.0 / 99.0).round() as u8)
}

/// The Matter mired span of a valid logical interval. `TemperatureInterval`
/// construction already guarantees that the span is representable.
pub(crate) fn mired_bounds(interval: TemperatureInterval) -> (u16, u16) {
    mired_span(interval.min(), interval.max())
        .expect("a valid temperature interval is mired-representable")
}

/// Coordinate conversion from the Matter mired scale. The unquantized kelvin
/// value is validated against the target's interval and quantized on the
/// native grid only once, at the source boundary.
pub(crate) fn kelvin_from_mireds(mireds: u16) -> f64 {
    1_000_000.0 / mireds as f64
}

/// Project a kelvin value into the Matter mired span of the published interval.
pub(crate) fn mireds_from_kelvin(
    interval: TemperatureInterval,
    kelvin: u32,
) -> Result<u16, CommandError> {
    if !interval.contains(kelvin) {
        return Err(CommandError::Rejected(
            "color temperature is outside its physical range".into(),
        ));
    }
    let (min, max) = mired_bounds(interval);
    Ok(((1_000_000.0 / kelvin as f64).round() as u16).clamp(min, max))
}

/// Celsius to the Matter temperature scale (0.01 °C). Range checks apply both
/// before and after rounding; the raw value is never clamped.
pub(crate) fn temperature_matter_value(celsius: f64) -> Option<i16> {
    if !celsius.is_finite() {
        return None;
    }
    if !(-273.15..=327.67).contains(&celsius) {
        return None;
    }
    let rounded = (celsius * 100.0).round();
    if !(-27_315.0..=32_767.0).contains(&rounded) {
        return None;
    }
    Some(rounded as i16)
}

/// Percentage to the Matter relative-humidity scale (0.01 %). Range checks
/// apply both before and after rounding; the raw value is never clamped.
pub(crate) fn humidity_matter_value(percent: f64) -> Option<u16> {
    if !percent.is_finite() {
        return None;
    }
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }
    let rounded = (percent * 100.0).round();
    if !(0.0..=10_000.0).contains(&rounded) {
        return None;
    }
    Some(rounded as u16)
}

pub(crate) fn temperature_matter_range(range: MeasurementRange) -> Option<(i16, i16)> {
    let min = temperature_matter_value(range.min())?;
    let max = temperature_matter_value(range.max())?;
    // MinMeasuredValue is at most 32766 and MaxMeasuredValue is at least
    // -27314; MeasuredValue itself uses the full signed range.
    (min <= 32_766 && max >= -27_314 && min < max).then_some((min, max))
}

pub(crate) fn humidity_matter_range(range: MeasurementRange) -> Option<(u16, u16)> {
    let min = humidity_matter_value(range.min())?;
    let max = humidity_matter_value(range.max())?;
    (min <= 9_999 && max >= 1 && min < max).then_some((min, max))
}
