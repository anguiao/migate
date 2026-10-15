// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Adapted from rs-matter 0.4.1 dm/clusters/app/level_control.rs,
// commit 3f3528ef5c492b6feb0a2e043264dc47e9da4340. Apache-2.0.
// Retains Task, Options validation, bounded step/move and quiet-report state.
// Device hooks are replaced by Function action tokens; time uses elapsed deadlines.
use super::{RemainingTime, should_continue, validate_options};
use rs_matter::{
    dm::clusters::decl::level_control::{MoveModeEnum, StepModeEnum},
    error::{Error, ErrorCode},
};

#[derive(Clone, Copy, Debug)]
pub(in crate::matter::lighting) enum Task {
    MoveToLevel {
        with_on_off: bool,
        start: u8,
        target: u8,
        transition_ms: u64,
    },
    Move {
        with_on_off: bool,
        start: u8,
        move_mode: MoveModeEnum,
        rate: u8,
    },
}
impl Task {
    pub fn duration(self) -> u64 {
        match self {
            Self::MoveToLevel {
                start,
                target,
                transition_ms,
                ..
            } => {
                if start == target {
                    0
                } else {
                    transition_ms
                }
            }
            Self::Move {
                start,
                move_mode,
                rate,
                ..
            } => {
                let target = match move_mode {
                    MoveModeEnum::Up => 254,
                    MoveModeEnum::Down => 1,
                };
                (start.abs_diff(target) as u64 * 1000).div_ceil(rate as u64)
            }
        }
    }
    pub fn frame(self, elapsed: u64) -> (u8, bool, Option<bool>) {
        match self {
            Self::MoveToLevel {
                with_on_off,
                start,
                target,
                transition_ms,
            } => {
                let end = elapsed >= self.duration();
                let steps = if end {
                    start.abs_diff(target) as u64
                } else {
                    elapsed * start.abs_diff(target) as u64 / transition_ms
                };
                let current = if target >= start {
                    start + steps as u8
                } else {
                    start - steps as u8
                };
                (
                    current,
                    end,
                    with_on_off.then_some(if end {
                        target > 1
                    } else {
                        start > 1 || target > start
                    }),
                )
            }
            Self::Move {
                with_on_off,
                start,
                move_mode,
                rate,
            } => {
                let steps = (elapsed as u128 * rate as u128 / 1000).min(254) as u8;
                let current = match move_mode {
                    MoveModeEnum::Up => start.saturating_add(steps).min(254),
                    MoveModeEnum::Down => start.saturating_sub(steps).max(1),
                };
                let end = elapsed >= self.duration();
                (
                    current,
                    end,
                    with_on_off.then_some(current > 1 || (!end && move_mode == MoveModeEnum::Up)),
                )
            }
        }
    }
}
#[derive(Default)]
pub(in crate::matter::lighting) struct LevelControlState {
    pub current_level: Option<u8>,
    pub on_level: Option<u8>,
    pub options: u8,
    pub remaining_time: RemainingTime,
}
impl LevelControlState {
    pub fn move_to_level(
        &self,
        mut level: u8,
        with_on_off: bool,
        transition_ds: Option<u16>,
        mask: u8,
        override_: u8,
        power: Option<bool>,
    ) -> Result<Option<Task>, Error> {
        if level > 254 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        level = level.clamp(1, 254);
        let transition_ms = transition_ds.unwrap_or(0) as u64 * 100;
        let start = if transition_ms == 0 {
            self.current_level.unwrap_or(level)
        } else {
            self.current_level.ok_or(ErrorCode::InvalidState)?
        };
        Ok(Some(Task::MoveToLevel {
            with_on_off,
            start,
            target: level,
            transition_ms,
        }))
    }
    pub fn move_level(
        &self,
        with_on_off: bool,
        mode: MoveModeEnum,
        rate: Option<u8>,
        mask: u8,
        override_: u8,
        power: Option<bool>,
    ) -> Result<Option<Task>, Error> {
        // Manufacturer-selected fastest planning rate, not DefaultMoveRate.
        const FASTEST_RATE: u8 = 50;
        let rate = rate.unwrap_or(FASTEST_RATE);
        if rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        Ok(Some(Task::Move {
            with_on_off,
            start: self.current_level.ok_or(ErrorCode::InvalidState)?,
            move_mode: mode,
            rate,
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &self,
        with_on_off: bool,
        mode: StepModeEnum,
        size: u8,
        transition: Option<u16>,
        mask: u8,
        override_: u8,
        power: Option<bool>,
    ) -> Result<Option<Task>, Error> {
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        let current = self.current_level.ok_or(ErrorCode::InvalidState)?;
        let target = match mode {
            StepModeEnum::Up => current.saturating_add(size).min(254),
            StepModeEnum::Down => current.saturating_sub(size).max(1),
        };
        let duration =
            transition.unwrap_or(0) as u64 * current.abs_diff(target) as u64 / size as u64;
        self.move_to_level(
            target,
            with_on_off,
            Some(duration as u16),
            mask,
            override_,
            power,
        )
    }
}
