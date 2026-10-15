use crate::xiaomi::capability::PropertyBinding;
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};

// Notifications have no command identifier. Keep only a short, fixed receipt history,
// never a permanent set of values that could hide later external commands.
const RETENTION: Duration = Duration::from_secs(3);
const CAPACITY: usize = 512;

struct Accepted {
    binding: PropertyBinding,
    value: Value,
    expires: Duration,
}

struct Pending {
    binding: PropertyBinding,
    value: Value,
    /// `None` until the native receipt arrives.
    expires: Option<Duration>,
    ticket: u64,
}

#[derive(Default)]
pub(super) struct LightFeedback {
    sent: Vec<(PropertyBinding, Value)>,
    history: VecDeque<Accepted>,
    /// Expected echoes of native writes that a member did not receive itself.
    expected: VecDeque<Pending>,
    next_ticket: u64,
}

impl LightFeedback {
    pub(super) fn sent(&mut self, properties: &[(PropertyBinding, Value)]) {
        self.sent = properties.to_vec();
    }

    /// Native-to-member expectations stay pending until the native receipt
    /// arrives, then live for the short accepted window. A slow receipt must
    /// not let its own expectation expire before the echo can arrive.
    pub(super) fn expect_pending(&mut self, properties: &[(PropertyBinding, Value)]) -> u64 {
        self.next_ticket += 1;
        let ticket = self.next_ticket;
        for (binding, value) in properties {
            self.expected.push_back(Pending {
                binding: *binding,
                value: value.clone(),
                expires: None,
                ticket,
            });
        }
        while self.expected.len() > CAPACITY {
            self.expected.pop_front();
        }
        ticket
    }

    pub(super) fn expect_ack(&mut self, ticket: u64, now: Duration) {
        for expected in &mut self.expected {
            if expected.ticket == ticket && expected.expires.is_none() {
                expected.expires = Some(now + RETENTION);
            }
        }
    }

    pub(super) fn clear_expected(&mut self, ticket: u64) {
        self.expected.retain(|expected| expected.ticket != ticket);
    }

    pub(super) fn complete(
        &mut self,
        accepted: impl Iterator<Item = PropertyBinding>,
        now: Duration,
    ) {
        self.prune(now);
        for binding in accepted {
            if let Some(index) = self.sent.iter().position(|(sent, _)| *sent == binding) {
                let (_, value) = self.sent.remove(index);
                if self.history.len() == CAPACITY {
                    self.history.pop_front();
                }
                self.history.push_back(Accepted {
                    binding,
                    value,
                    expires: now + RETENTION,
                });
            }
        }
        self.cancel();
    }

    pub(super) fn cancel(&mut self) {
        self.sent.clear();
    }

    pub(super) fn clear_all(&mut self) {
        self.sent.clear();
        self.history.clear();
        self.expected.clear();
    }

    pub(super) fn matches(
        &mut self,
        binding: PropertyBinding,
        value: &Value,
        now: Duration,
    ) -> bool {
        self.prune(now);
        self.history
            .iter()
            .any(|accepted| accepted.binding == binding && &accepted.value == value)
            || self.expected.iter().any(|expected| {
                expected.binding == binding
                    && &expected.value == value
                    && expected.expires.is_none_or(|expires| now < expires)
            })
            || self
                .sent
                .iter()
                .any(|(sent, target)| *sent == binding && target == value)
    }

    fn prune(&mut self, now: Duration) {
        self.history.retain(|accepted| now < accepted.expires);
        self.expected
            .retain(|expected| expected.expires.is_none_or(|expires| now < expires));
    }

    pub(super) fn is_empty(&mut self, now: Duration) -> bool {
        self.prune(now);
        self.sent.is_empty() && self.history.is_empty() && self.expected.is_empty()
    }
}

#[cfg(test)]
#[path = "tests/feedback.rs"]
mod tests;
