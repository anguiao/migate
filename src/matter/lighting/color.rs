// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// CT conversion adapted from rs-matter 0.4.1 color_control.rs, commit
// 3f3528ef5c492b6feb0a2e043264dc47e9da4340.
//! Matter color coordinate conversions and encoded sRGB projection.

use crate::device::{Rgb, lighting::hs::Hs};

pub(super) const XY_MAX: u16 = 0xfeff;
const HUE_PERIOD: f64 = 254.0;
const SATURATION_MAX: f64 = 254.0;

/// Planckian-locus approximation adapted from rs-matter's CT conversion.
/// This projects a known white-light state when starting a color command.
pub(super) fn temperature_to_xy(mireds: u16) -> (u16, u16) {
    let t = 1_000_000.0 / f64::from(mireds);
    let t2 = t * t;
    let u = (0.860117757 + 1.54118254e-4 * t + 1.28641212e-7 * t2)
        / (1.0 + 8.42420235e-4 * t + 7.08145163e-7 * t2);
    let v = (0.317398726 + 4.22806245e-5 * t + 4.20481691e-8 * t2)
        / (1.0 - 2.89741816e-5 * t + 1.61456053e-7 * t2);
    let denominator = 2.0 * u - 8.0 * v + 4.0;
    (
        (3.0 * u / denominator * 65536.0)
            .round()
            .clamp(0.0, f64::from(XY_MAX)) as u16,
        (2.0 * v / denominator * 65536.0)
            .round()
            .clamp(0.0, f64::from(XY_MAX)) as u16,
    )
}

// The upstream linear RGB/XYZ pair is replaced by the paired sRGB D65 matrices
// from https://www.w3.org/TR/css-color-4/#color-conversion-code. Matter XY
// uses 1/65536 units; XY_MAX limits valid values, not the conversion scale.
pub(super) fn rgb_to_xy(rgb: Rgb) -> Option<(u16, u16)> {
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
pub(super) fn xy_to_rgb(x: u16, y: u16) -> Rgb {
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

/// HSV over encoded sRGB, with brightness kept in the independent level channel.
/// Achromatic RGB has no unique hue; the caller can retain its accepted hue.
pub(super) fn rgb_to_hs(rgb: Rgb) -> Hs {
    let (r, g, b) = (rgb.red as f64, rgb.green as f64, rgb.blue as f64);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if delta == 0.0 {
        return Hs {
            hue: 0.0,
            saturation: 0.0,
        };
    }
    let sextant = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    Hs {
        hue: sextant * HUE_PERIOD / 6.0,
        saturation: delta / max * SATURATION_MAX,
    }
}

pub(super) fn hs_to_rgb(hs: Hs) -> Rgb {
    let sextant = hs.hue.rem_euclid(HUE_PERIOD) / HUE_PERIOD * 6.0;
    let chroma = hs.saturation / SATURATION_MAX;
    let second = chroma * (1.0 - (sextant.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match sextant as u8 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let channel = |value: f64| ((value + 1.0 - chroma) * 255.0).round() as u8;
    Rgb {
        red: channel(r),
        green: channel(g),
        blue: channel(b),
    }
}

#[cfg(test)]
mod tests;
