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
