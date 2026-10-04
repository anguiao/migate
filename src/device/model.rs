use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceKey {
    pub source: String,
    pub id: String,
}

impl DeviceKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Category {
    Outlet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    OnOff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDescription {
    pub key: DeviceKey,
    pub category: Category,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Power {
    Unknown,
    On,
    Off,
}

impl Power {
    pub fn value(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }
}

impl From<bool> for Power {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    Initializing,
    Available,
    Unavailable(String),
}

impl Availability {
    pub(super) fn command_result(&self) -> Result<(), CommandError> {
        match self {
            Self::Available => Ok(()),
            Self::Initializing => Err(CommandError::Unavailable("initializing".into())),
            Self::Unavailable(reason) => Err(CommandError::Unavailable(reason.clone())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSnapshot {
    pub power: Power,
    pub availability: Availability,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unsupported,
    Unavailable(String),
    UnknownState,
    Timeout,
    Rejected(String),
    SendFailed(String),
    Cancelled,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("device does not support on/off commands"),
            Self::Unavailable(reason) => write!(formatter, "device unavailable: {reason}"),
            Self::UnknownState => formatter.write_str("cannot toggle an unknown power state"),
            Self::Timeout => formatter.write_str("command acceptance timed out"),
            Self::Rejected(reason) => write!(formatter, "command rejected: {reason}"),
            Self::SendFailed(reason) => write!(formatter, "command send failed: {reason}"),
            Self::Cancelled => formatter.write_str("command cancelled"),
        }
    }
}

impl Error for CommandError {}
