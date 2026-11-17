/// Hue direction in the caller's circular coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
    Up,
    Down,
    Shortest,
    Longest,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hs {
    pub hue: f64,
    pub saturation: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AxisKind {
    Hue,
    Saturation,
}

fn hue_delta(start: f64, target: f64, direction: Direction, period: f64) -> f64 {
    let up = (target - start).rem_euclid(period);
    let down = if up == 0.0 { 0.0 } else { up - period };
    match direction {
        Direction::Up => up,
        Direction::Down => down,
        Direction::Shortest => {
            if up <= period / 2.0 {
                up
            } else {
                down
            }
        }
        Direction::Longest => {
            if up == 0.0 {
                period
            } else if up >= period / 2.0 {
                up
            } else {
                down
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Coordinates {
    Linear,
    Circular(f64),
    Bounded(f64),
}
impl Coordinates {
    fn project(self, value: f64) -> f64 {
        match self {
            Self::Linear => value,
            Self::Circular(period) => value.rem_euclid(period),
            Self::Bounded(maximum) => value.clamp(0.0, maximum),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Hold(f64),
    MoveTo {
        start: f64,
        delta: f64,
        target: f64,
        transition_ms: u64,
        coordinates: Coordinates,
    },
    Move {
        start: f64,
        rate: f64,
        coordinates: Coordinates,
    },
}

/// Both axes share one elapsed offset and deadline implementation. Resuming
/// preserves the original curve rather than restarting from rounded RGB.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Axis {
    motion: Motion,
    elapsed_offset: u64,
}
impl Axis {
    fn new(motion: Motion) -> Self {
        Self {
            motion,
            elapsed_offset: 0,
        }
    }

    fn frame(self, elapsed: u64) -> f64 {
        let elapsed = elapsed.saturating_add(self.elapsed_offset);
        match self.motion {
            Motion::Hold(value) => value,
            Motion::MoveTo {
                start,
                delta,
                target,
                transition_ms,
                coordinates,
            } => {
                if elapsed >= transition_ms || delta == 0.0 {
                    target
                } else {
                    coordinates.project(start + delta * elapsed as f64 / transition_ms as f64)
                }
            }
            Motion::Move {
                start,
                rate,
                coordinates,
            } => coordinates.project(start + rate * elapsed as f64 / 1000.0),
        }
    }

    fn duration(self) -> u64 {
        let duration = match self.motion {
            Motion::Hold(_) => 0,
            Motion::MoveTo {
                delta,
                transition_ms,
                ..
            } => {
                if delta == 0.0 {
                    0
                } else {
                    transition_ms
                }
            }
            Motion::Move {
                start,
                rate,
                coordinates: Coordinates::Bounded(maximum),
            } => {
                let distance = if rate > 0.0 { maximum - start } else { start };
                (distance * 1000.0 / rate.abs()).ceil() as u64
            }
            Motion::Move { .. } => return u64::MAX,
        };
        duration.saturating_sub(self.elapsed_offset)
    }

    fn resume(mut self, elapsed: u64) -> Self {
        self.elapsed_offset = self.elapsed_offset.saturating_add(elapsed);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HueAxis(Axis);
impl HueAxis {
    pub(crate) fn hold(value: f64) -> Self {
        Self(Axis::new(Motion::Hold(value)))
    }

    pub(crate) fn move_to(
        start: f64,
        target: f64,
        direction: Direction,
        transition_ms: u64,
        period: f64,
    ) -> Self {
        Self(Axis::new(Motion::MoveTo {
            start,
            delta: hue_delta(start, target, direction, period),
            target,
            transition_ms,
            coordinates: Coordinates::Circular(period),
        }))
    }

    pub(crate) fn step(start: f64, delta: f64, transition_ms: u64, period: f64) -> Self {
        Self(Axis::new(Motion::MoveTo {
            start,
            delta,
            target: (start + delta).rem_euclid(period),
            transition_ms,
            coordinates: Coordinates::Circular(period),
        }))
    }

    pub(crate) fn move_at(start: f64, rate: f64, period: f64) -> Self {
        Self(Axis::new(Motion::Move {
            start,
            rate,
            coordinates: Coordinates::Circular(period),
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SaturationAxis(Axis);
impl SaturationAxis {
    pub(crate) fn hold(value: f64) -> Self {
        Self(Axis::new(Motion::Hold(value)))
    }

    pub(crate) fn move_to(start: f64, target: f64, transition_ms: u64) -> Self {
        Self(Axis::new(Motion::MoveTo {
            start,
            delta: target - start,
            target,
            transition_ms,
            coordinates: Coordinates::Linear,
        }))
    }

    pub(crate) fn move_at(start: f64, rate: f64, maximum: f64) -> Self {
        Self(Axis::new(Motion::Move {
            start,
            rate,
            coordinates: Coordinates::Bounded(maximum),
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HsTask {
    pub hue: HueAxis,
    pub saturation: SaturationAxis,
}
impl HsTask {
    pub(crate) fn stationary(value: Hs) -> Self {
        Self {
            hue: HueAxis::hold(value.hue),
            saturation: SaturationAxis::hold(value.saturation),
        }
    }

    pub(crate) fn frame(self, elapsed_ms: u64) -> (Hs, bool) {
        let duration = self.duration();
        (
            Hs {
                hue: self.hue.0.frame(elapsed_ms),
                saturation: self.saturation.0.frame(elapsed_ms),
            },
            duration != u64::MAX && elapsed_ms >= duration,
        )
    }

    pub(crate) fn duration(self) -> u64 {
        self.hue.0.duration().max(self.saturation.0.duration())
    }

    pub(super) fn is_achromatic(self) -> bool {
        self.saturation.0.duration() == 0 && self.saturation.0.frame(0) == 0.0
    }

    pub(super) fn retain(&mut self, previous: Self, elapsed_ms: u64, axis: AxisKind) {
        let previous = previous.resume(elapsed_ms);
        match axis {
            AxisKind::Hue => self.hue = previous.hue,
            AxisKind::Saturation => self.saturation = previous.saturation,
        }
    }

    fn resume(self, elapsed_ms: u64) -> Self {
        Self {
            hue: HueAxis(self.hue.0.resume(elapsed_ms)),
            saturation: SaturationAxis(self.saturation.0.resume(elapsed_ms)),
        }
    }
}

#[cfg(test)]
mod tests;
