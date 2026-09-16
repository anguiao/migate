use std::{error::Error as StdError, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MqttErrorKind {
    InvalidInput,
    Protocol,
    Unauthorized,
    Network,
    Timeout,
    Disconnected,
    Capacity,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MqttError {
    operation: &'static str,
    kind: MqttErrorKind,
}

impl MqttError {
    pub(super) fn new(operation: &'static str, kind: MqttErrorKind) -> Self {
        Self { operation, kind }
    }

    pub fn guard_failed() -> Self {
        Self::new("check MQTT send permission", MqttErrorKind::Disconnected)
    }

    pub(crate) fn superseded() -> Self {
        Self::new("select MQTT subscriptions", MqttErrorKind::Superseded)
    }

    pub fn operation(&self) -> &'static str {
        self.operation
    }

    pub fn kind(&self) -> &MqttErrorKind {
        &self.kind
    }
}

impl fmt::Display for MqttError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Failed to {}: ", self.operation)?;
        formatter.write_str(match self.kind {
            MqttErrorKind::InvalidInput => "invalid input",
            MqttErrorKind::Protocol => "invalid MQTT response",
            MqttErrorKind::Unauthorized => "MQTT authorization was rejected",
            MqttErrorKind::Network => "network request failed",
            MqttErrorKind::Timeout => "network request timed out",
            MqttErrorKind::Disconnected => "MQTT connection closed",
            MqttErrorKind::Capacity => "MQTT session capacity was exceeded",
            MqttErrorKind::Superseded => "subscription selection was superseded",
        })
    }
}

impl StdError for MqttError {}
