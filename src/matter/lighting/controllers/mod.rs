// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2025-2026 Project CHIP Authors
// Licensed under Apache-2.0: https://www.apache.org/licenses/LICENSE-2.0
// Options and quiet-report state adapted from rs-matter 0.4.1 Level/Color,
// commit 3f3528ef5c492b6feb0a2e043264dc47e9da4340; see adjacent LICENSE.
pub(super) mod clock;
pub(super) mod color;
pub(super) mod hs;
pub(super) mod level;

use rs_matter::error::{Error, ErrorCode};
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

#[derive(Default)]
pub(super) struct RemainingTime {
    pub value: u16,
    short_transition: bool,
}
impl RemainingTime {
    // Adapted from LevelControlState::write_remaining_time_quietly and
    // ColorControlState::write_remaining_time_quietly; saturation avoids wrap.
    pub fn write(&mut self, millis: u64, start: bool, maximum: u16) -> bool {
        // Matter 1.6 Level §1.6.6.3 / Color §3.2.7.4 suppress RemainingTime
        // reports for an entire transition shorter than one second. A longer
        // transition keeps its end/Stop report even when the remainder is short.
        if start {
            self.short_transition = millis > 0 && millis < 1000;
        }
        let new = millis.div_ceil(100).min(maximum as u64) as u16;
        let old = self.value;
        self.value = new;
        !self.short_transition
            && ((new == 0 && old != 0)
                || (old == 0 && new > 10)
                || (start && new.abs_diff(old) > 10))
    }
}
