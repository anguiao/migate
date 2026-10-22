mod connection;
mod dispatch;
mod driver;
mod mqtt;
mod request;

pub use connection::connect;
#[cfg(test)]
pub(super) use connection::start;
pub use driver::SessionDriver;
pub use request::{
    DeviceAccess, DeviceEntry, RequestOutcome, RequestStage, RequestTimeout, SendDecision,
    SendHooks, SessionHandle,
};

use crate::xiaomi::capability::PropertyBinding;
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::time::Duration;

const LIMIT: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CentralEvent {
    PropertySnapshot {
        did: String,
        binding: PropertyBinding,
        value: serde_json::Value,
        update_time: Option<i64>,
    },
    PropertyValue {
        did: String,
        binding: PropertyBinding,
        value: serde_json::Value,
        update_time: Option<i64>,
    },
    DeviceListChanged {
        dids: Vec<String>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct Timing {
    pub(super) request: Duration,
    pub(super) keepalive: Option<Duration>,
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
