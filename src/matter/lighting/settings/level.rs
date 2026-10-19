use super::super::Projection;
use super::{should_continue, validate_options};
use crate::lighting::level::{Direction, Task};
use rs_matter::{
    dm::clusters::decl::level_control::{MoveModeEnum, StepModeEnum},
    error::{Error, ErrorCode},
};

/// Writable Matter settings. Builders consume feedback explicitly; trajectories
/// live in the shared scheduler.
#[derive(Default)]
pub(in crate::matter::lighting) struct LevelControlState {
    pub on_level: Option<u8>,
    pub options: u8,
}
impl LevelControlState {
    pub fn move_to_level(
        &self,
        mut level: u8,
        with_on_off: bool,
        transition_ds: Option<u16>,
        mask: u8,
        override_: u8,
        feedback: Projection,
    ) -> Result<Option<Task>, Error> {
        if level > 254 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(feedback.power, self.options, mask, override_)? {
            return Ok(None);
        }
        level = level.clamp(1, 254);
        let transition_ms = transition_ds.unwrap_or(0) as u64 * 100;
        let start = if transition_ms == 0 {
            feedback.level.unwrap_or(level)
        } else {
            feedback.level.ok_or(ErrorCode::InvalidState)?
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
        feedback: Projection,
    ) -> Result<Option<Task>, Error> {
        // Manufacturer-selected fastest planning rate, not DefaultMoveRate.
        const FASTEST_RATE: u8 = 50;
        let rate = rate.unwrap_or(FASTEST_RATE);
        if rate == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(feedback.power, self.options, mask, override_)? {
            return Ok(None);
        }
        let move_mode = match mode {
            MoveModeEnum::Up => Direction::Up,
            MoveModeEnum::Down => Direction::Down,
        };
        Ok(Some(Task::Move {
            with_on_off,
            start: feedback.level.ok_or(ErrorCode::InvalidState)?,
            move_mode,
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
        feedback: Projection,
    ) -> Result<Option<Task>, Error> {
        if size == 0 {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if with_on_off {
            validate_options(mask, override_)?;
        } else if !should_continue(feedback.power, self.options, mask, override_)? {
            return Ok(None);
        }
        let current = feedback.level.ok_or(ErrorCode::InvalidState)?;
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
            feedback,
        )
    }
}
