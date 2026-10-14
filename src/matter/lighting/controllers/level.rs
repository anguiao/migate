// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Matter-side LevelControl interpretation; physical frames are shared.
use crate::lighting::planning::LevelPlan;

use super::RemainingTime;
use crate::lighting::controllers::MoveDirection;
use rs_matter::{
    dm::clusters::decl::level_control::{MoveModeEnum, StepModeEnum},
    error::{Error, ErrorCode},
};

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
        level: u8,
        with_on_off: bool,
        transition_ds: Option<u16>,
        mask: u8,
        override_: u8,
        power: Option<bool>,
    ) -> Result<Option<LevelPlan>, Error> {
        if level > 254 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            super::validate_options(mask, override_)?;
        } else if !super::should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        Ok(Some(LevelPlan::MoveTo {
            with_on_off,
            target: level.clamp(1, 254),
            transition_ms: transition_ds.unwrap_or(0) as u64 * 100,
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
    ) -> Result<Option<LevelPlan>, Error> {
        // Manufacturer-selected fastest planning rate, not DefaultMoveRate.
        const FASTEST_RATE: u8 = 50;
        let rate = rate.unwrap_or(FASTEST_RATE);
        if rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            super::validate_options(mask, override_)?;
        } else if !super::should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        let direction = match mode {
            MoveModeEnum::Up => MoveDirection::Up,
            MoveModeEnum::Down => MoveDirection::Down,
        };
        Ok(Some(LevelPlan::Move {
            with_on_off,
            direction,
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
    ) -> Result<Option<LevelPlan>, Error> {
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            super::validate_options(mask, override_)?;
        } else if !super::should_continue(power, self.options, mask, override_)? {
            return Ok(None);
        }
        Ok(Some(LevelPlan::Step {
            with_on_off,
            direction: match mode {
                StepModeEnum::Up => MoveDirection::Up,
                StepModeEnum::Down => MoveDirection::Down,
            },
            size,
            transition,
        }))
    }
}
