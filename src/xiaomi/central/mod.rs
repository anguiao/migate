mod session;
mod tls;
mod wire;
pub use session::{
    CentralEvent, DeviceAccess, DeviceEntry, RequestOutcome, RequestStage, RequestTimeout,
    SendDecision, SendHooks, SessionDriver, SessionHandle, connect,
};
pub use tls::connect_tls;
pub use wire::{Frames as MqttFrames, MipsMessage, decode_mips, encode_mips};

use std::{error::Error, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CentralError {
    InvalidInput,
    InvalidCredentials,
    Tls,
    Io,
    Disconnected,
    Protocol,
    Timeout,
    Rejected(i64),
    Cancelled,
    Withdrawn,
    QueueFull,
}

impl fmt::Display for CentralError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("Invalid gateway request input"),
            Self::InvalidCredentials => {
                f.write_str("Gateway client credentials are invalid or expired")
            }
            Self::Tls => f.write_str("Gateway TLS authentication failed"),
            Self::Io => f.write_str("Gateway transport failed"),
            Self::Disconnected => f.write_str("Gateway disconnected"),
            Self::Protocol => f.write_str("Invalid gateway protocol message"),
            Self::Timeout => f.write_str("Gateway request timed out"),
            Self::Rejected(code) => write!(f, "Gateway rejected request with code {code}"),
            Self::Cancelled => f.write_str("Gateway session was cancelled"),
            Self::Withdrawn => f.write_str("Unsent gateway request was withdrawn"),
            Self::QueueFull => f.write_str("Gateway request queue is full"),
        }
    }
}

impl Error for CentralError {}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) use tests::support as test_support;
