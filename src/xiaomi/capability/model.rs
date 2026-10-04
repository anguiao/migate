use std::{error::Error, fmt};

use crate::device::{DeviceKey, FunctionDescription};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedFunction {
    pub description: FunctionDescription,
    pub power: PropertyBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDevice {
    pub key: DeviceKey,
    pub functions: Vec<ParsedFunction>,
    pub refresh_trigger: Option<PropertyBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
    MissingFunction(&'static str),
    IncompatibleModel(&'static str),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet switch must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "outlet switch lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
            Self::MissingFunction(id) => {
                write!(formatter, "required outlet binding is missing: {id}")
            }
            Self::IncompatibleModel(model) => {
                write!(formatter, "unsupported MIoT description for {model}")
            }
        }
    }
}

impl Error for CapabilityError {}
