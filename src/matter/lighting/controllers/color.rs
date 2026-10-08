// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Adapted from rs-matter 0.4.1 dm/clusters/app/color_control.rs,
// commit 3f3528ef5c492b6feb0a2e043264dc47e9da4340. Apache-2.0.
// Retains CT/XY Task interpolation, signed rates and bounded steps. HS/Scenes
// are outside the supported profile. Accepted snapshots replace device-hook state.
use super::RemainingTime;
use crate::device::Rgb;

pub(in crate::matter::lighting) const XY_MAX: u16 = 0xfeff;
#[derive(Clone, Copy, Debug)]
pub(in crate::matter::lighting) enum Task {
    XyMoveTo {
        start_x: u16,
        start_y: u16,
        target_x: u16,
        target_y: u16,
        transition_ms: u64,
    },
    XyMove {
        start_x: u16,
        start_y: u16,
        rate_x: i16,
        rate_y: i16,
    },
    ColorTemperatureMoveTo {
        start: u16,
        target_mireds: u16,
        transition_ms: u64,
    },
    ColorTemperatureMove {
        start: u16,
        rate_mireds_per_sec: i32,
        min_mireds: u16,
        max_mireds: u16,
    },
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::matter::lighting) enum Target {
    Xy(u16, u16),
    Temperature(u16),
}
fn axis_duration(start: u16, rate: i32, lo: u16, hi: u16) -> u64 {
    if rate == 0 {
        return 0;
    }
    let distance = if rate > 0 {
        hi.saturating_sub(start)
    } else {
        start.saturating_sub(lo)
    };
    (distance as u64 * 1000).div_ceil(rate.unsigned_abs() as u64)
}
fn interpolate(start: u16, target: u16, elapsed: u64, total: u64) -> u16 {
    if elapsed >= total {
        return target;
    }
    // The upstream signed integer interpolation, with full elapsed precision.
    (start as i128 + (target as i128 - start as i128) * elapsed as i128 / total as i128) as u16
}
impl Task {
    pub fn duration(self) -> u64 {
        match self {
            Self::XyMoveTo {
                start_x,
                start_y,
                target_x,
                target_y,
                transition_ms,
            } => {
                if (start_x, start_y) == (target_x, target_y) {
                    0
                } else {
                    transition_ms
                }
            }
            Self::ColorTemperatureMoveTo {
                start,
                target_mireds,
                transition_ms,
            } => {
                if start == target_mireds {
                    0
                } else {
                    transition_ms
                }
            }
            Self::XyMove {
                start_x,
                start_y,
                rate_x,
                rate_y,
            } => axis_duration(start_x, rate_x as i32, 0, XY_MAX).max(axis_duration(
                start_y,
                rate_y as i32,
                0,
                XY_MAX,
            )),
            Self::ColorTemperatureMove {
                start,
                rate_mireds_per_sec,
                min_mireds,
                max_mireds,
            } => axis_duration(start, rate_mireds_per_sec, min_mireds, max_mireds),
        }
    }
    pub fn frame(self, elapsed: u64) -> (Target, bool) {
        let end = elapsed >= self.duration();
        let moving = |start: u16, rate: i32, lo: u16, hi: u16| {
            (start as i128 + rate as i128 * elapsed as i128 / 1000).clamp(lo as i128, hi as i128)
                as u16
        };
        let target = match self {
            Self::XyMoveTo {
                start_x,
                start_y,
                target_x,
                target_y,
                transition_ms,
            } => Target::Xy(
                interpolate(start_x, target_x, elapsed, transition_ms),
                interpolate(start_y, target_y, elapsed, transition_ms),
            ),
            Self::XyMove {
                start_x,
                start_y,
                rate_x,
                rate_y,
            } => Target::Xy(
                moving(start_x, rate_x as i32, 0, XY_MAX),
                moving(start_y, rate_y as i32, 0, XY_MAX),
            ),
            Self::ColorTemperatureMoveTo {
                start,
                target_mireds,
                transition_ms,
            } => Target::Temperature(interpolate(start, target_mireds, elapsed, transition_ms)),
            Self::ColorTemperatureMove {
                start,
                rate_mireds_per_sec,
                min_mireds,
                max_mireds,
            } => Target::Temperature(if rate_mireds_per_sec > 0 {
                moving(start, rate_mireds_per_sec, 0, max_mireds.max(start))
            } else {
                moving(start, rate_mireds_per_sec, min_mireds.min(start), u16::MAX)
            }),
        };
        (target, end)
    }
}
#[derive(Default)]
pub(in crate::matter::lighting) struct ColorControlState {
    pub xy: Option<(u16, u16)>,
    pub color_temperature_mireds: Option<u16>,
    pub mode: Option<crate::device::LightMode>,
    pub options: u8,
    pub remaining_time: RemainingTime,
}

// The upstream linear RGB/XYZ pair is replaced by the paired sRGB D65 matrices
// from https://www.w3.org/TR/css-color-4/#color-conversion-code. Matter XY
// uses 1/65536 units; XY_MAX limits valid values, not the conversion scale.
pub(in crate::matter::lighting) fn rgb_to_xy(rgb: Rgb) -> Option<(u16, u16)> {
    let linear = |value: u8| {
        let c = value as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(rgb.red), linear(rgb.green), linear(rgb.blue));
    let x = r * 506752.0 / 1228815.0 + g * 87881.0 / 245763.0 + b * 12673.0 / 70218.0;
    let y = r * 87098.0 / 409605.0 + g * 175762.0 / 245763.0 + b * 12673.0 / 175545.0;
    let z = r * 7918.0 / 409605.0 + g * 87881.0 / 737289.0 + b * 1001167.0 / 1053270.0;
    let sum = x + y + z;
    (sum > 0.0).then(|| {
        (
            (x / sum * 65536.0).round().clamp(0.0, XY_MAX as f64) as u16,
            (y / sum * 65536.0).round().clamp(0.0, XY_MAX as f64) as u16,
        )
    })
}
pub(in crate::matter::lighting) fn xy_to_rgb(x: u16, y: u16) -> Rgb {
    if y == 0 {
        return Rgb {
            red: 0,
            green: 0,
            blue: 0,
        };
    }
    let (x, y) = (x as f64 / 65536.0, y as f64 / 65536.0);
    let (xx, yy, zz) = (x / y, 1.0, (1.0 - x - y) / y);
    let r = (xx * 12831.0 / 3959.0 - yy * 329.0 / 214.0 - zz * 1974.0 / 3959.0).max(0.0);
    let g =
        (-xx * 851781.0 / 878810.0 + yy * 1648619.0 / 878810.0 + zz * 36519.0 / 878810.0).max(0.0);
    let b = (xx * 705.0 / 12673.0 - yy * 2585.0 / 12673.0 + zz * 705.0 / 667.0).max(0.0);
    let maximum = r.max(g).max(b);
    let encoded = |linear: f64| {
        let c = if maximum > 0.0 { linear / maximum } else { 0.0 };
        let gamma = if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (gamma * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Rgb {
        red: encoded(r),
        green: encoded(g),
        blue: encoded(b),
    }
}
