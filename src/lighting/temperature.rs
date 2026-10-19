//! Matter mired representation conversions.
//!
//! The device and source models keep only valid kelvin intervals and native
//! quantization grids. Converting between kelvin and the Matter mired scale is
//! a representation concern owned by this boundary. Mired to kelvin keeps full
//! precision so the native grid is quantized exactly once, at the Xiaomi send
//! boundary; encoding limits and their validation stay in the Matter handlers.

use crate::device::{CommandError, TemperatureInterval};

/// The Matter mired span of a valid logical interval.
pub(crate) fn mired_bounds(interval: TemperatureInterval) -> (u16, u16) {
    let min = 1_000_000_u64.div_ceil(interval.max() as u64);
    let max = 1_000_000_u64 / interval.min() as u64;
    (min as u16, max as u16)
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
