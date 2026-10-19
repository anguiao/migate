//! Test-only inspection and state transitions for `DeviceWork`.
//!
//! Production callers use the concrete attach/prepare/observer operations in
//! `work.rs`; these helpers keep private preparation state out of the public
//! surface while existing scenarios keep asserting the same facts.

use super::DeviceWork;
use crate::xiaomi::capability::PropertyBinding;

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn test_observer_is_local(&self) -> bool {
        matches!(
            self.observer(),
            Some(super::super::notifications::NotificationSource::Local(_))
        )
    }
    pub(in crate::xiaomi::adapter) fn test_observer_is_cloud(&self) -> bool {
        matches!(
            self.observer(),
            Some(super::super::notifications::NotificationSource::Cloud(_))
        )
    }
    #[cfg(test)]
    pub(in crate::xiaomi::adapter) fn test_notify(&self, binding: PropertyBinding, value: bool) {
        self.notify_value(binding, &serde_json::Value::Bool(value), false, true);
    }
    pub(in crate::xiaomi::adapter) fn test_local_exhausted(&self) -> bool {
        self.local_exhausted()
    }
    pub(in crate::xiaomi::adapter) fn test_cloud_exhausted(&self) -> bool {
        self.cloud_exhausted()
    }
    pub(in crate::xiaomi::adapter) fn test_local_subscribed_contains(
        &self,
        binding: PropertyBinding,
    ) -> bool {
        self.local
            .borrow()
            .prepare
            .subscribed
            .contains(&(binding.siid, binding.piid))
    }
    pub(in crate::xiaomi::adapter) fn test_local_subscribed_pair(
        &self,
        siid: u32,
        piid: u32,
    ) -> bool {
        self.local
            .borrow()
            .prepare
            .subscribed
            .contains(&(siid, piid))
    }
    pub(in crate::xiaomi::adapter) fn test_local_subscribed_len(&self) -> usize {
        self.local.borrow().prepare.subscribed.len()
    }
    pub(in crate::xiaomi::adapter) fn test_local_subscribed_empty(&self) -> bool {
        self.local.borrow().prepare.subscribed.is_empty()
    }
    pub(in crate::xiaomi::adapter) fn test_local_attached(&self) -> bool {
        self.local.borrow().connection.is_some()
    }
    pub(in crate::xiaomi::adapter) fn test_cloud_subscribed_empty(&self) -> bool {
        self.cloud.borrow().prepare.subscribed.is_empty()
    }
    pub(in crate::xiaomi::adapter) fn test_cloud_preparing(&self) -> bool {
        self.cloud.borrow().prepare.running
    }
    /// Reproduce a real preparation cancellation while a queued handshake is
    /// still outstanding: advance its receipt generation and release `running`.
    pub(in crate::xiaomi::adapter) fn test_retire_cloud_preparation(&self) {
        self.retire_cloud_preparation();
    }
}
