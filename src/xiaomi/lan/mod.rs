mod discovery;
mod packet;
mod protocol;
mod session;
mod target;

pub use session::{
    LanEventArgument, LanEventArguments, LanEvidence, LanHandle, LanNotification, LanProperty,
    LanPropertyRead, LanPropertyWrite, LanReadOutcome, LanSendGuard, LanSession, LanSubscription,
    LanWriteOutcome,
};
pub use target::LanTarget;

use std::{error::Error as StdError, fmt};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LanErrorKind {
    InvalidInput,
    Protocol,
    Business(i64),
    Timeout,
    Transport,
    NotAuthenticated,
    Unsupported,
    RateLimited,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanError {
    operation: &'static str,
    kind: LanErrorKind,
    may_have_been_sent: bool,
}

impl LanError {
    fn new(operation: &'static str, kind: LanErrorKind) -> Self {
        Self {
            operation,
            kind,
            may_have_been_sent: false,
        }
    }

    pub fn kind(&self) -> &LanErrorKind {
        &self.kind
    }

    pub fn may_have_been_sent(&self) -> bool {
        self.may_have_been_sent
    }

    pub(crate) fn session_stopped() -> Self {
        Self::new("start LAN session", LanErrorKind::Transport)
    }

    fn after_send(mut self, sent: bool) -> Self {
        if sent && !matches!(self.kind, LanErrorKind::Business(_)) {
            self.may_have_been_sent = true;
        }
        self
    }
}

impl fmt::Display for LanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Failed to {}: ", self.operation)?;
        match self.kind {
            LanErrorKind::InvalidInput => formatter.write_str("invalid input"),
            LanErrorKind::Protocol => formatter.write_str("invalid LAN response"),
            LanErrorKind::Business(code) => write!(formatter, "device code {code}"),
            LanErrorKind::Timeout => formatter.write_str("LAN request timed out"),
            LanErrorKind::Transport => formatter.write_str("LAN transport failed"),
            LanErrorKind::NotAuthenticated => formatter.write_str("LAN peer is not authenticated"),
            LanErrorKind::Unsupported => formatter.write_str("LAN operation is unsupported"),
            LanErrorKind::RateLimited => formatter.write_str("LAN probe is rate limited"),
            LanErrorKind::Cancelled => formatter.write_str("LAN operation was cancelled"),
        }
    }
}

impl StdError for LanError {}

#[cfg(test)]
mod tests;
pub use discovery::{LanDiscovery, LanHelloCandidate, LanSubscriptionHint};
#[cfg(test)]
pub(crate) use tests::{
    receive_request as receive_request_for_test, reject_native_authentication_probe,
    reply as reply_for_test, session_pair,
};
