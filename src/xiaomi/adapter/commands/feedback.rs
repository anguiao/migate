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

#[derive(Default)]
pub(super) struct LightFeedback {
    sent: Vec<(PropertyBinding, Value)>,
    history: VecDeque<Accepted>,
}

impl LightFeedback {
    pub(super) fn sent(&mut self, properties: &[(PropertyBinding, Value)]) {
        self.sent = properties.to_vec();
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
            || self
                .sent
                .iter()
                .any(|(sent, target)| *sent == binding && target == value)
    }

    fn prune(&mut self, now: Duration) {
        self.history.retain(|accepted| now < accepted.expires);
    }

    pub(super) fn is_empty(&mut self, now: Duration) -> bool {
        self.prune(now);
        self.sent.is_empty() && self.history.is_empty()
    }
}

#[cfg(test)]
#[path = "tests/feedback.rs"]
mod tests;
