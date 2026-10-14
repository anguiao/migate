// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Matter-side protocol interpretation. Options, OnLevel and the quiet-report
// RemainingTime rule stay here; the physical trajectories live in
// `crate::lighting::controllers`.
pub(super) mod clock;
pub(super) mod color;
pub(super) mod hs;
pub(super) mod level;

use crate::lighting::Transition;
use rs_matter::error::{Error, ErrorCode};

/// Quiet-report state for the Matter RemainingTime attributes. The shared
/// executor exposes current timing; the Matter-specific short-transition
/// report rule and the encoded value stay at the protocol entry.
#[derive(Default)]
pub(super) struct RemainingTime {
    pub value: u16,
    transition: Option<Transition>,
}
impl RemainingTime {
    // Adapted from LevelControlState::write_remaining_time_quietly and
    // ColorControlState::write_remaining_time_quietly; saturation avoids wrap.
    pub(super) fn write(&mut self, transition: Option<Transition>, now: u64, maximum: u16) -> bool {
        // Matter 1.6 Level §1.6.6.3 / Color §3.2.7.4 suppress RemainingTime
        // reports for an entire transition shorter than one second. A longer
        // transition keeps its end/Stop report even when the remainder is short.
        let start = transition.is_some() && transition != self.transition;
        let short = transition
            .or(self.transition)
            .is_some_and(|transition| transition.duration > 0 && transition.duration < 1000);
        self.transition = transition;
        let millis = transition.map_or(0, |transition| transition.remaining_ms(now));
        let new = millis.div_ceil(100).min(maximum as u64) as u16;
        let old = self.value;
        self.value = new;
        !short
            && ((new == 0 && old != 0)
                || (old == 0 && new > 10)
                || (start && new.abs_diff(old) > 10))
    }
}

pub(super) fn validate_options(mask: u8, override_: u8) -> Result<(), Error> {
    if (mask | override_) & !1 != 0 {
        return Err(ErrorCode::ConstraintError.into());
    }
    Ok(())
}
pub(super) fn should_continue(
    power: Option<bool>,
    options: u8,
    mask: u8,
    override_: u8,
) -> Result<bool, Error> {
    // The profile implements ExecuteIfOff only. An ordinary command cannot
    // enable omitted CoupleColorTempToLevel behavior through a temporary override.
    validate_options(mask, override_)?;
    // rs-matter Options/OptionsMask/OptionsOverride processing.
    let execute_if_off = if mask & 1 != 0 {
        override_ & 1 != 0
    } else {
        options & 1 != 0
    };
    match power {
        Some(true) => Ok(true),
        Some(false) => Ok(execute_if_off),
        None if execute_if_off => Ok(true),
        None => Err(ErrorCode::InvalidState.into()),
    }
}
