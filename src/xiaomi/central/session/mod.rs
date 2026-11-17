mod connection;
mod dispatch;
mod driver;
mod mqtt;
mod request;

pub(in crate::xiaomi) use connection::connect;
pub(in crate::xiaomi) use driver::SessionDriver;
pub(in crate::xiaomi) use request::{
    DeviceAccess, DeviceEntry, RequestOutcome, RequestStage, RequestTimeout, SendDecision,
    SendHooks, SessionHandle,
};

use crate::xiaomi::capability::{EventBinding, PropertyBinding};
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::time::Duration;

const LIMIT: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::xiaomi) enum CentralEvent {
    EventOccurred {
        did: String,
        binding: EventBinding,
        arguments: Vec<serde_json::Value>,
        source_time: Option<i64>,
    },
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
pub(in crate::xiaomi) mod tests;
