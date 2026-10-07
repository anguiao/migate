mod connection;
mod driver;
mod request;
mod tls;

pub use connection::connect_notifications;
pub use driver::NotificationDriver;
pub use request::NotificationHandle;

use crate::xiaomi::capability::PropertyBinding;
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::{error::Error, fmt, time::Duration};

const LIMIT: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationEvent {
    Property {
        did: String,
        binding: PropertyBinding,
        value: bool,
    },
    Online {
        did: String,
        online: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationError {
    InvalidInput,
    Tls,
    Io,
    Timeout,
    Protocol,
    Disconnected,
    Rejected(u8),
    Unauthorized,
    Cancelled,
    QueueFull,
}

impl fmt::Display for NotificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("Invalid cloud notification input"),
            Self::Tls => f.write_str("Cloud notification TLS verification failed"),
            Self::Io => f.write_str("Cloud notification transport failed"),
            Self::Timeout => f.write_str("Cloud notification request timed out"),
            Self::Protocol => f.write_str("Invalid cloud MQTT protocol message"),
            Self::Disconnected => f.write_str("Cloud notification connection closed"),
            Self::Rejected(code) => write!(f, "Cloud MQTT rejected request with code {code}"),
            Self::Unauthorized => f.write_str("Cloud MQTT authorization was rejected"),
            Self::Cancelled => f.write_str("Cloud notification session was cancelled"),
            Self::QueueFull => f.write_str("Cloud notification request queue is full"),
        }
    }
}

impl Error for NotificationError {}

#[derive(Clone, Copy)]
pub(super) struct Timing {
    request: Duration,
    keepalive: Option<Duration>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(10),
            keepalive: Some(Duration::from_secs(60)),
        }
    }
}

pub(super) trait Transport: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Transport for T {}

#[cfg(test)]
pub(crate) mod tests;
