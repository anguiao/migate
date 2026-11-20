use super::*;
use crate::device::{
    AirConditionerCommand as Command, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerSnapshot, AirConditionerTarget, CommandError, TemperatureGrid,
    air_conditioner::AirConditionerConstraints,
};
use crate::xiaomi::capability::PropertyBinding;
use futures_lite::future::poll_once;

use self::support::*;

mod commands;
mod mcn04;
mod sources;
mod support;
