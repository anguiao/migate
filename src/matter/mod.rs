mod air_conditioner;
mod bridge;
mod bridged_info;
mod curtain;
mod fan;
mod fan_control;
mod heater;
mod lighting;
mod model;
mod on_off;
mod pairing;
mod sensors;
mod shape;
mod states;
mod storage;
mod thermostat;
pub(crate) mod units;

pub(crate) use bridge::Advertisement;
pub(crate) use bridge::{Bridge, LabelWriter};
pub use pairing::PairingEvent;
pub(crate) use storage::subscription_keys;

pub(crate) use model::{
    MatterConfiguration, PublishedEntry, PublishedFunction, PublishedIdentity, PublishedRelay,
};
pub(crate) use sensors::measurement_mapping_issue;
pub(crate) use shape::fixed_shape_with_battery;

fn command_error(
    function: &crate::device::Function,
    command: impl std::fmt::Debug,
    error: crate::device::CommandError,
) -> rs_matter::error::Error {
    use crate::device::CommandError;
    use rs_matter::error::ErrorCode;

    log::warn!(
        "Matter {}/{} {command:?} failed: {error}",
        function.function_key().device.id,
        function.function_key().id,
    );
    match error {
        CommandError::UnknownState | CommandError::InvalidInState => ErrorCode::InvalidInState,
        CommandError::Constraint => ErrorCode::ConstraintError,
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        CommandError::Unavailable(_)
        | CommandError::Rejected(_)
        | CommandError::SendFailed(_)
        | CommandError::Cancelled
        | CommandError::Superseded
        | CommandError::Stopped => ErrorCode::Failure,
    }
    .into()
}

#[cfg(test)]
pub(crate) mod tests;
