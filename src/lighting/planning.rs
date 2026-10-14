// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
//! Resolve normalized light commands against a caller-selected current state.

use super::controllers::{MoveDirection, color, hs, level};
use crate::device::CommandError;

/// A validated level command, before selecting an individual light's start.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LevelPlan {
    MoveTo {
        target: u8,
        transition_ms: u64,
        with_on_off: bool,
    },
    Move {
        direction: MoveDirection,
        rate: u8,
        with_on_off: bool,
    },
    Step {
        direction: MoveDirection,
        size: u8,
        transition: Option<u16>,
        with_on_off: bool,
    },
    Stop,
}

/// A color command with protocol enums already mapped and effective
/// temperature bounds fixed at the protocol entry.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ColorPlan {
    TemperatureMoveTo {
        target_mireds: u16,
        transition_ms: u64,
    },
    TemperatureMove {
        direction: MoveDirection,
        rate: u16,
        min_mireds: u16,
        max_mireds: u16,
    },
    TemperatureStep {
        direction: MoveDirection,
        size: u16,
        transition_ms: u64,
        min_mireds: u16,
        max_mireds: u16,
    },
    HueAndSaturationMoveTo {
        target: hs::Hs,
        transition_ms: u64,
    },
    HueMoveTo {
        target: f64,
        direction: hs::HueDirection,
        transition_ms: u64,
    },
    HueMove {
        rate: f64,
    },
    HueStep {
        delta: f64,
        transition_ms: u64,
    },
    SaturationMoveTo {
        target: f64,
        transition_ms: u64,
    },
    SaturationMove {
        rate: f64,
    },
    SaturationStep {
        delta: f64,
        transition_ms: u64,
    },
    XyMoveTo {
        x: u16,
        y: u16,
        transition_ms: u64,
    },
    XyMove {
        rate_x: i16,
        rate_y: i16,
    },
    XyStep {
        delta_x: i16,
        delta_y: i16,
        transition_ms: u64,
    },
    /// Stop the color dimension on the trigger member set.
    Stop,
    /// Stop only members whose color dimension currently holds an HS move.
    StopHs,
}

impl LevelPlan {
    pub(crate) fn resolve(self, current: Option<u8>) -> Result<level::Task, CommandError> {
        match self {
            Self::MoveTo {
                target,
                transition_ms,
                with_on_off,
            } => {
                let start = if transition_ms == 0 {
                    current.unwrap_or(target)
                } else {
                    current.ok_or(CommandError::UnknownState)?
                };
                Ok(level::Task::MoveToLevel {
                    with_on_off,
                    start,
                    target,
                    transition_ms,
                })
            }
            Self::Move {
                direction,
                rate,
                with_on_off,
            } => Ok(level::Task::Move {
                with_on_off,
                start: current.ok_or(CommandError::UnknownState)?,
                direction,
                rate,
            }),
            Self::Step {
                direction,
                size,
                transition,
                with_on_off,
            } => {
                let start = current.ok_or(CommandError::UnknownState)?;
                let target = match direction {
                    MoveDirection::Up => start.saturating_add(size).min(254),
                    MoveDirection::Down => start.saturating_sub(size).max(1),
                };
                // Preserve the requested rate when a bound clips the distance.
                let duration =
                    transition.unwrap_or(0) as u64 * start.abs_diff(target) as u64 / size as u64;
                Self::MoveTo {
                    target,
                    transition_ms: duration * 100,
                    with_on_off,
                }
                .resolve(current)
            }
            Self::Stop => Err(CommandError::Unsupported),
        }
    }
}

/// State selection stays at the entry: single lights use the protocol view;
/// group members use fresh physical state and their own running trajectory.
pub(crate) struct ColorState {
    pub temperature_mireds: Option<u16>,
    pub xy: Option<(u16, u16)>,
    pub hs: Option<hs::Hs>,
    pub continuing_hs: Option<hs::HsTask>,
}

impl ColorState {
    fn continuing_hs(&self) -> Result<(hs::HsTask, hs::Hs), CommandError> {
        let start = self.hs.ok_or(CommandError::UnknownState)?;
        Ok((
            self.continuing_hs
                .unwrap_or_else(|| hs::HsTask::stationary(start)),
            start,
        ))
    }
}

impl ColorPlan {
    pub(crate) fn is_temperature(self) -> bool {
        matches!(
            self,
            Self::TemperatureMoveTo { .. }
                | Self::TemperatureMove { .. }
                | Self::TemperatureStep { .. }
        )
    }

    pub(crate) fn resolve(self, state: ColorState) -> Result<color::Task, CommandError> {
        Ok(match self {
            Self::TemperatureMoveTo {
                target_mireds,
                transition_ms,
            } => {
                let start = if transition_ms == 0 {
                    state.temperature_mireds.unwrap_or(target_mireds)
                } else {
                    state.temperature_mireds.ok_or(CommandError::UnknownState)?
                };
                color::Task::ColorTemperatureMoveTo {
                    start,
                    target_mireds,
                    transition_ms,
                }
            }
            Self::TemperatureMove {
                direction,
                rate,
                min_mireds,
                max_mireds,
            } => {
                let start = state.temperature_mireds.ok_or(CommandError::UnknownState)?;
                let rate_mireds_per_sec = match direction {
                    MoveDirection::Up => rate as i32,
                    MoveDirection::Down => -(rate as i32),
                };
                color::Task::ColorTemperatureMove {
                    start,
                    rate_mireds_per_sec,
                    min_mireds,
                    max_mireds,
                }
            }
            Self::TemperatureStep {
                direction,
                size,
                transition_ms,
                min_mireds,
                max_mireds,
            } => {
                let start = state.temperature_mireds.ok_or(CommandError::UnknownState)?;
                let target_mireds =
                    color::step_mireds(start, size, direction, min_mireds, max_mireds);
                // Temperature Step keeps its rate; XY Step keeps its duration.
                let transition_ms =
                    (transition_ms * start.abs_diff(target_mireds) as u64).div_ceil(size as u64);
                color::Task::ColorTemperatureMoveTo {
                    start,
                    target_mireds,
                    transition_ms,
                }
            }
            Self::HueAndSaturationMoveTo {
                target,
                transition_ms,
            } => color::Task::Hs(if transition_ms == 0 {
                hs::HsTask::stationary(target)
            } else {
                let start = state.hs.ok_or(CommandError::UnknownState)?;
                hs::HsTask {
                    hue: hs::HueAxis::move_to(
                        start.hue,
                        target.hue,
                        hs::HueDirection::Shortest,
                        transition_ms,
                    ),
                    saturation: hs::SaturationAxis::move_to(
                        start.saturation,
                        target.saturation,
                        transition_ms,
                    ),
                }
            }),
            Self::HueMoveTo {
                target,
                direction,
                transition_ms,
            } => {
                let (mut task, start) = state.continuing_hs()?;
                task.hue = hs::HueAxis::move_to(start.hue, target, direction, transition_ms);
                color::Task::Hs(task)
            }
            Self::HueMove { rate } => {
                let (mut task, start) = state.continuing_hs()?;
                task.hue = hs::HueAxis::move_at(start.hue, rate);
                color::Task::Hs(task)
            }
            Self::HueStep {
                delta,
                transition_ms,
            } => {
                let (mut task, start) = state.continuing_hs()?;
                task.hue = hs::HueAxis::step(start.hue, delta, transition_ms);
                color::Task::Hs(task)
            }
            Self::SaturationMoveTo {
                target,
                transition_ms,
            } => {
                let (mut task, start) = state.continuing_hs()?;
                task.saturation =
                    hs::SaturationAxis::move_to(start.saturation, target, transition_ms);
                color::Task::Hs(task)
            }
            Self::SaturationMove { rate } => {
                let (mut task, start) = state.continuing_hs()?;
                task.saturation = hs::SaturationAxis::move_at(start.saturation, rate);
                color::Task::Hs(task)
            }
            Self::SaturationStep {
                delta,
                transition_ms,
            } => {
                let (mut task, start) = state.continuing_hs()?;
                task.saturation = hs::SaturationAxis::move_to(
                    start.saturation,
                    (start.saturation + delta).clamp(0.0, 254.0),
                    transition_ms,
                );
                color::Task::Hs(task)
            }
            Self::XyMoveTo {
                x,
                y,
                transition_ms,
            } => {
                let (start_x, start_y) = if transition_ms == 0 {
                    (x, y)
                } else {
                    state.xy.ok_or(CommandError::UnknownState)?
                };
                color::Task::XyMoveTo {
                    start_x,
                    start_y,
                    target_x: x,
                    target_y: y,
                    transition_ms,
                }
            }
            Self::XyMove { rate_x, rate_y } => {
                let (start_x, start_y) = state.xy.ok_or(CommandError::UnknownState)?;
                color::Task::XyMove {
                    start_x,
                    start_y,
                    rate_x,
                    rate_y,
                }
            }
            Self::XyStep {
                delta_x,
                delta_y,
                transition_ms,
            } => {
                let (start_x, start_y) = state.xy.ok_or(CommandError::UnknownState)?;
                let x = (start_x as i32 + delta_x as i32).clamp(0, color::XY_MAX as i32) as u16;
                let y = (start_y as i32 + delta_y as i32).clamp(0, color::XY_MAX as i32) as u16;
                return Self::XyMoveTo {
                    x,
                    y,
                    transition_ms,
                }
                .resolve(state);
            }
            Self::Stop | Self::StopHs => return Err(CommandError::Unsupported),
        })
    }
}
