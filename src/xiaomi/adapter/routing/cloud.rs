use super::super::{cloud::CloudSync, model::AdapterError};
use super::*;
use crate::xiaomi::{
    auth::{AuthenticationState, FailureReason},
    cloud::CloudError,
};
use std::time::Duration;

impl Context {
    pub(in crate::xiaomi::adapter) fn cloud_authenticated(&self) -> bool {
        self.cloud_authentication().is_ok()
    }
    pub(in crate::xiaomi::adapter) fn cloud_authentication(&self) -> Result<(), String> {
        if !self.cloud_scope_valid.get() {
            return Err("account/home scope not validated".into());
        }
        if !self.token_usable() {
            return Err("credentials unavailable, expired or rejected".into());
        }
        let live = self.live.borrow();
        if !live.credentials.as_ref().is_some_and(|credentials| {
            live.scope
                .as_ref()
                .is_some_and(|scope| scope.uid == credentials.uid)
        }) {
            return Err("account/home identity mismatch".into());
        }
        Ok(())
    }
    pub(in crate::xiaomi::adapter) fn token_usable(&self) -> bool {
        !matches!(
            *self.cloud_sync.borrow(),
            CloudSync::Rejected(_) | CloudSync::Paused
        ) && self
            .live
            .borrow()
            .credentials
            .as_ref()
            .is_some_and(|credentials| credentials.tokens.expires_at > self.clock.unix_now())
    }
    pub(in crate::xiaomi::adapter) fn current_token(&self, token: &str) -> bool {
        self.token_usable()
            && self
                .live
                .borrow()
                .credentials
                .as_ref()
                .is_some_and(|credentials| credentials.tokens.access_token == token)
    }
    pub(in crate::xiaomi::adapter) async fn token_retired(&self, token: &str) {
        loop {
            let listener = self.credentials_changed.listen();
            if !self.current_token(token) {
                return;
            }
            listener.await;
        }
    }
    /// Token rejection and HTTP failure share one account maintenance entry point.
    pub(in crate::xiaomi::adapter) fn cloud_error(
        &self,
        token: &str,
        did: Option<&str>,
        error: &CloudError,
    ) -> Result<(), AdapterError> {
        if error.is_unauthorized() {
            self.token_rejected(token, FailureReason::Cloud(error.clone()))?;
        } else if error.is_temporary() {
            if !self.http_paused.replace(true) {
                self.sync_not_before.set(Some(
                    self.clock.now() + self.sync_backoff.borrow_mut().failed(),
                ));
                self.request_sync();
            }
        } else if let Some(did) = did {
            let mut live = self.live.borrow_mut();
            if let Some(slot) = live.devices.get_mut(did) {
                if error.is_device_offline()
                    && let Some(access) = &mut slot.cloud_access
                {
                    access.online = false;
                }
                if error.is_device_forbidden() {
                    slot.cloud_forbidden = true;
                }
            }
        }
        self.update_all_availability();
        let reason = if error.is_unauthorized() {
            "cloud access token rejected"
        } else if error.is_temporary() {
            "cloud HTTP request failed"
        } else if error.is_device_offline() {
            "cloud device reported offline"
        } else if error.is_device_forbidden() {
            "cloud device access forbidden"
        } else {
            "cloud request failed"
        };
        self.update_routing(&format!("{reason}: {error}"));
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
    /// Temporary property-read errors are retried by the reader, not account sync.
    pub(in crate::xiaomi::adapter) fn cloud_property_read_error(
        &self,
        token: &str,
        work: &DeviceWork,
        error: &CloudError,
    ) -> Result<(), AdapterError> {
        if error.is_temporary() {
            return Ok(());
        }
        let (current_token, current_work) = {
            let live = self.live.borrow();
            (
                live.credentials
                    .as_ref()
                    .is_some_and(|credentials| credentials.tokens.access_token == token),
                !work.life.is_closed()
                    && live
                        .devices
                        .get(&work.key.id)
                        .and_then(|slot| slot.admitted.as_deref())
                        .is_some_and(|current| std::ptr::eq(current, work)),
            )
        };
        if !current_token {
            return Ok(());
        }
        if error.is_unauthorized() {
            self.cloud_error(token, None, error)
        } else if current_work {
            self.cloud_error(token, Some(&work.key.id), error)
        } else {
            Ok(())
        }
    }
    pub(in crate::xiaomi::adapter) fn token_rejected(
        &self,
        token: &str,
        reason: FailureReason,
    ) -> Result<(), AdapterError> {
        let current = self.store.xiaomi().load()?;
        if current
            .as_ref()
            .is_some_and(|record| record.tokens.access_token == token)
        {
            let mut sync = self.cloud_sync.borrow_mut();
            match &*sync {
                CloudSync::Refreshed(fresh) if fresh == token => {
                    *sync = CloudSync::Paused;
                    self.status.borrow_mut().authentication.authentication =
                        AuthenticationState::SignInRequired(reason);
                    self.status_changed.notify(usize::MAX);
                }
                CloudSync::Rejected(_) | CloudSync::Paused => {}
                _ => {
                    *sync = CloudSync::Rejected(token.into());
                    self.request_sync();
                }
            }
        }
        self.update_all_availability();
        self.update_routing("cloud access token rejected");
        self.changed.notify(usize::MAX);
        self.credentials_changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
    pub(in crate::xiaomi::adapter) fn sync_failed(&self) -> Duration {
        let due = self.clock.now() + self.sync_backoff.borrow_mut().failed();
        self.sync_not_before.set(Some(due));
        due
    }
}
