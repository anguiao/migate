use crate::device::Rgb;
use rs_matter::dm::clusters::decl::color_control::DirectionEnum;

const HUE_PERIOD: f64 = 254.0;
const SATURATION_MAX: f64 = 254.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::matter::lighting) struct Hs {
    pub hue: f64,
    pub saturation: f64,
}

/// Preserve the selected path, including the long way around the hue circle.
pub(in crate::matter::lighting) fn hue_delta(
    start: f64,
    target: f64,
    direction: DirectionEnum,
) -> f64 {
    let up = (target - start).rem_euclid(HUE_PERIOD);
    let down = if up == 0.0 { 0.0 } else { up - HUE_PERIOD };
    match direction {
        DirectionEnum::Up => up,
        DirectionEnum::Down => down,
        DirectionEnum::Shortest => {
            if up <= HUE_PERIOD / 2.0 {
                up
            } else {
                down
            }
        }
        DirectionEnum::Longest => {
            if up == 0.0 {
                HUE_PERIOD
            } else if up >= HUE_PERIOD / 2.0 {
                up
            } else {
                down
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::matter::lighting) enum HueAxis {
    Hold(f64),
    MoveTo {
        start: f64,
        delta: f64,
        target: f64,
        transition_ms: u64,
        elapsed_offset: u64,
    },
    Move {
        start: f64,
        rate: f64,
        elapsed_offset: u64,
    },
}

impl HueAxis {
    pub fn move_to(start: f64, target: f64, direction: DirectionEnum, transition_ms: u64) -> Self {
        Self::MoveTo {
            start,
            delta: hue_delta(start, target, direction),
            target,
            transition_ms,
            elapsed_offset: 0,
        }
    }

    pub fn step(start: f64, delta: f64, transition_ms: u64) -> Self {
        Self::MoveTo {
            start,
            delta,
            target: (start + delta).rem_euclid(HUE_PERIOD),
            transition_ms,
            elapsed_offset: 0,
        }
    }

    pub fn move_at(start: f64, rate: f64) -> Self {
        Self::Move {
            start,
            rate,
            elapsed_offset: 0,
        }
    }

    fn frame(self, elapsed: u64) -> f64 {
        match self {
            Self::Hold(value) => value,
            Self::MoveTo {
                start,
                delta,
                target,
                transition_ms,
                elapsed_offset,
            } => {
                let elapsed = elapsed.saturating_add(elapsed_offset);
                if elapsed >= transition_ms || delta == 0.0 {
                    target
                } else {
                    (start + delta * elapsed as f64 / transition_ms as f64).rem_euclid(HUE_PERIOD)
                }
            }
            Self::Move {
                start,
                rate,
                elapsed_offset,
            } => {
                let elapsed = elapsed.saturating_add(elapsed_offset);
                (start + rate * elapsed as f64 / 1000.0).rem_euclid(HUE_PERIOD)
            }
        }
    }

    fn duration(self) -> u64 {
        match self {
            Self::Hold(_) => 0,
            Self::MoveTo {
                delta,
                transition_ms,
                elapsed_offset,
                ..
            } => {
                if delta == 0.0 {
                    0
                } else {
                    transition_ms.saturating_sub(elapsed_offset)
                }
            }
            Self::Move { .. } => u64::MAX,
        }
    }

    fn resume(mut self, elapsed: u64) -> Self {
        match &mut self {
            Self::Hold(_) => {}
            Self::MoveTo { elapsed_offset, .. } | Self::Move { elapsed_offset, .. } => {
                *elapsed_offset = elapsed_offset.saturating_add(elapsed);
            }
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::matter::lighting) enum SaturationAxis {
    Hold(f64),
    MoveTo {
        start: f64,
        target: f64,
        transition_ms: u64,
        elapsed_offset: u64,
    },
    Move {
        start: f64,
        rate: f64,
        elapsed_offset: u64,
    },
}

impl SaturationAxis {
    pub fn move_to(start: f64, target: f64, transition_ms: u64) -> Self {
        Self::MoveTo {
            start,
            target,
            transition_ms,
            elapsed_offset: 0,
        }
    }

    pub fn move_at(start: f64, rate: f64) -> Self {
        Self::Move {
            start,
            rate,
            elapsed_offset: 0,
        }
    }

    fn frame(self, elapsed: u64) -> f64 {
        match self {
            Self::Hold(value) => value,
            Self::MoveTo {
                start,
                target,
                transition_ms,
                elapsed_offset,
            } => {
                let elapsed = elapsed.saturating_add(elapsed_offset);
                if elapsed >= transition_ms {
                    target
                } else {
                    start + (target - start) * elapsed as f64 / transition_ms as f64
                }
            }
            Self::Move {
                start,
                rate,
                elapsed_offset,
            } => {
                let elapsed = elapsed.saturating_add(elapsed_offset);
                (start + rate * elapsed as f64 / 1000.0).clamp(0.0, SATURATION_MAX)
            }
        }
    }

    fn duration(self) -> u64 {
        match self {
            Self::Hold(_) => 0,
            Self::MoveTo {
                start,
                target,
                transition_ms,
                elapsed_offset,
            } => {
                if start == target {
                    0
                } else {
                    transition_ms.saturating_sub(elapsed_offset)
                }
            }
            Self::Move {
                start,
                rate,
                elapsed_offset,
            } => {
                let distance = if rate > 0.0 {
                    SATURATION_MAX - start
                } else {
                    start
                };
                ((distance * 1000.0 / rate.abs()).ceil() as u64).saturating_sub(elapsed_offset)
            }
        }
    }

    fn resume(mut self, elapsed: u64) -> Self {
        match &mut self {
            Self::Hold(_) => {}
            Self::MoveTo { elapsed_offset, .. } | Self::Move { elapsed_offset, .. } => {
                *elapsed_offset = elapsed_offset.saturating_add(elapsed);
            }
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::matter::lighting) struct HsTask {
    pub hue: HueAxis,
    pub saturation: SaturationAxis,
}

impl HsTask {
    pub fn stationary(value: Hs) -> Self {
        Self {
            hue: HueAxis::Hold(value.hue),
            saturation: SaturationAxis::Hold(value.saturation),
        }
    }

    pub fn frame(self, elapsed_ms: u64) -> (Hs, bool) {
        let duration = self.duration();
        (
            Hs {
                hue: self.hue.frame(elapsed_ms),
                saturation: self.saturation.frame(elapsed_ms),
            },
            duration != u64::MAX && elapsed_ms >= duration,
        )
    }

    pub fn duration(self) -> u64 {
        self.hue.duration().max(self.saturation.duration())
    }

    pub fn is_achromatic(self) -> bool {
        self.saturation.duration() == 0 && self.saturation.frame(0) == 0.0
    }

    /// A new single-axis command retains the other axis's original timeline.
    pub fn resume(self, elapsed_ms: u64) -> Self {
        Self {
            hue: self.hue.resume(elapsed_ms),
            saturation: self.saturation.resume(elapsed_ms),
        }
    }
}

/// HSV over encoded sRGB, with brightness kept in the independent level channel.
/// Achromatic RGB has no unique hue; the caller can retain its accepted hue.
pub(in crate::matter::lighting) fn rgb_to_hs(rgb: Rgb) -> Hs {
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

pub(in crate::matter::lighting) fn hs_to_rgb(hs: Hs) -> Rgb {
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
#[path = "tests/hs.rs"]
mod tests;
