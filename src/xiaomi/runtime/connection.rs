use super::{CONNECTION_RETRY_MAX, XiaomiSafeFailureCode};
use crate::xiaomi::runtime::TransportStartupError;
use event_listener::Event;
use futures_lite::future;
use std::{cell::Cell, rc::Rc, time::Duration};

#[derive(Clone)]
pub(super) struct ConnectionSetup {
    active: Rc<Cell<usize>>,
    available: Rc<Event>,
    limit: usize,
}

pub(super) struct ConnectionSetupSlot(ConnectionSetup);

impl ConnectionSetup {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            active: Rc::new(Cell::new(0)),
            available: Rc::new(Event::new()),
            limit,
        }
    }

    pub(super) async fn acquire(
        &self,
        stopped: &Cell<bool>,
        changed: &Event,
    ) -> Option<ConnectionSetupSlot> {
        loop {
            let listener = self.available.listen();
            let changed = changed.listen();
            if stopped.get() {
                return None;
            }
            if self.active.get() < self.limit {
                self.active.set(self.active.get() + 1);
                return Some(ConnectionSetupSlot(self.clone()));
            }
            future::or(listener, changed).await;
        }
    }
}

impl Drop for ConnectionSetupSlot {
    fn drop(&mut self) {
        self.0.active.set(self.0.active.get().saturating_sub(1));
        self.0.available.notify(1);
    }
}

pub(super) fn advance_retry(current: &mut Duration, initial: Duration) -> Duration {
    let delay = *current;
    *current = current
        .saturating_mul(2)
        .min(CONNECTION_RETRY_MAX)
        .max(initial);
    delay
}

pub(super) fn startup_failure_code(error: &TransportStartupError) -> XiaomiSafeFailureCode {
    match error {
        TransportStartupError::Timeout => XiaomiSafeFailureCode::Timeout,
        TransportStartupError::Mqtt(error) => mqtt_failure_code(error.kind()),
        TransportStartupError::Gateway(error) => match error.kind() {
            crate::xiaomi::gateway::GatewayErrorKind::Protocol
            | crate::xiaomi::gateway::GatewayErrorKind::InvalidInput => {
                XiaomiSafeFailureCode::Protocol
            }
            crate::xiaomi::gateway::GatewayErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
            crate::xiaomi::gateway::GatewayErrorKind::Business(code) => {
                XiaomiSafeFailureCode::Rejected(*code)
            }
            crate::xiaomi::gateway::GatewayErrorKind::Transport => {
                XiaomiSafeFailureCode::Unavailable
            }
            crate::xiaomi::gateway::GatewayErrorKind::Superseded => {
                XiaomiSafeFailureCode::Unavailable
            }
        },
    }
}

pub(super) fn mqtt_failure_code(
    kind: &crate::xiaomi::mqtt::MqttErrorKind,
) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::mqtt::MqttErrorKind::Unauthorized => XiaomiSafeFailureCode::Unauthorized,
        crate::xiaomi::mqtt::MqttErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::mqtt::MqttErrorKind::Protocol
        | crate::xiaomi::mqtt::MqttErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::mqtt::MqttErrorKind::Network
        | crate::xiaomi::mqtt::MqttErrorKind::Disconnected
        | crate::xiaomi::mqtt::MqttErrorKind::Capacity
        | crate::xiaomi::mqtt::MqttErrorKind::Superseded => XiaomiSafeFailureCode::Unavailable,
    }
}

pub(super) fn gateway_failure_code(
    kind: &crate::xiaomi::gateway::GatewayErrorKind,
) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::gateway::GatewayErrorKind::Protocol
        | crate::xiaomi::gateway::GatewayErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::gateway::GatewayErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::gateway::GatewayErrorKind::Business(code) => {
            XiaomiSafeFailureCode::Rejected(*code)
        }
        crate::xiaomi::gateway::GatewayErrorKind::Transport
        | crate::xiaomi::gateway::GatewayErrorKind::Superseded => {
            XiaomiSafeFailureCode::Unavailable
        }
    }
}

pub(super) fn lan_failure_code(kind: &crate::xiaomi::lan::LanErrorKind) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::lan::LanErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::lan::LanErrorKind::Protocol
        | crate::xiaomi::lan::LanErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::lan::LanErrorKind::Business(code) => XiaomiSafeFailureCode::Rejected(*code),
        crate::xiaomi::lan::LanErrorKind::Transport
        | crate::xiaomi::lan::LanErrorKind::NotAuthenticated
        | crate::xiaomi::lan::LanErrorKind::Unsupported
        | crate::xiaomi::lan::LanErrorKind::RateLimited
        | crate::xiaomi::lan::LanErrorKind::Cancelled => XiaomiSafeFailureCode::Unavailable,
    }
}
