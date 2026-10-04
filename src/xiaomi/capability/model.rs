use std::{error::Error, fmt};

use crate::device::DeviceDescription;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedOutlet {
    pub description: DeviceDescription,
    pub main_power: PropertyBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet main power must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "outlet main power lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
        }
    }
}

impl Error for CapabilityError {}
