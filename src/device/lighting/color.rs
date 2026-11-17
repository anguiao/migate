// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Adapted from rs-matter 0.4.1 dm/clusters/app/color_control.rs,
// commit 3f3528ef5c492b6feb0a2e043264dc47e9da4340. Apache-2.0.
// Retains CT/XY interpolation; HS adds independent hue/saturation trajectories.
// Accepted snapshots replace device-hook state. Scenes remain outside this profile.
use super::hs;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Task {
    Hs(hs::HsTask),
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
        maximum: u16,
    },
    TemperatureMoveTo {
        start: u16,
        target: u16,
        transition_ms: u64,
    },
    TemperatureMove {
        start: u16,
        rate_per_sec: i32,
        min: u16,
        max: u16,
    },
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Target {
    Hs(hs::Hs),
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
    pub(crate) fn duration(self) -> u64 {
        match self {
            Self::Hs(task) => task.duration(),
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
            Self::TemperatureMoveTo {
                start,
                target,
                transition_ms,
            } => {
                if start == target {
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
                maximum,
            } => axis_duration(start_x, rate_x as i32, 0, maximum).max(axis_duration(
                start_y,
                rate_y as i32,
                0,
                maximum,
            )),
            Self::TemperatureMove {
                start,
                rate_per_sec,
                min,
                max,
            } => axis_duration(start, rate_per_sec, min, max),
        }
    }
    pub(crate) fn frame(self, elapsed: u64) -> (Target, bool) {
        if let Self::Hs(task) = self {
            let (target, end) = task.frame(elapsed);
            return (Target::Hs(target), end);
        }
        let end = elapsed >= self.duration();
        let moving = |start: u16, rate: i32, lo: u16, hi: u16| {
            (start as i128 + rate as i128 * elapsed as i128 / 1000).clamp(lo as i128, hi as i128)
                as u16
        };
        let target = match self {
            Self::Hs(_) => unreachable!(),
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
                maximum,
            } => Target::Xy(
                moving(start_x, rate_x as i32, 0, maximum),
                moving(start_y, rate_y as i32, 0, maximum),
            ),
            Self::TemperatureMoveTo {
                start,
                target,
                transition_ms,
            } => Target::Temperature(interpolate(start, target, elapsed, transition_ms)),
            Self::TemperatureMove {
                start,
                rate_per_sec,
                min,
                max,
            } => Target::Temperature(if rate_per_sec > 0 {
                moving(start, rate_per_sec, 0, max.max(start))
            } else {
                moving(start, rate_per_sec, min.min(start), u16::MAX)
            }),
        };
        (target, end)
    }
}
