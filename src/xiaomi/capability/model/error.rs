use std::{error::Error, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    UnsupportedLightGroup,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
    MissingFunction(&'static str),
    MissingMeasurement(&'static str),
    InvalidMeasurement(&'static str),
    IncompatibleModel(&'static str),
    InvalidLight(&'static str),
    InvalidGroup(&'static str),
    InvalidValue(&'static str),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::UnsupportedLightGroup => {
                formatter.write_str("Xiaomi light groups are not supported")
            }
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet switch must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "required property lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
            Self::MissingFunction(id) => {
                write!(formatter, "required binding is missing: {id}")
            }
            Self::MissingMeasurement(id) => {
                write!(formatter, "required measurement is missing: {id}")
            }
            Self::InvalidMeasurement(field) => {
                write!(formatter, "invalid measurement capability: {field}")
            }
            Self::InvalidLight(field) => write!(formatter, "invalid light capability: {field}"),
            Self::InvalidGroup(field) => write!(formatter, "invalid native light group: {field}"),
            Self::InvalidValue(field) => write!(formatter, "invalid property value: {field}"),
            Self::IncompatibleModel(model) => {
                write!(formatter, "unsupported MIoT description for {model}")
            }
        }
    }
}

impl Error for CapabilityError {}
