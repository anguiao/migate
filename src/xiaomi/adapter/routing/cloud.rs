use super::super::{credentials::HaSync, model::AdapterError};
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
        if self.lan_scope_invalid.get() {
            return Err("account/home scope revoked".into());
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
        !matches!(*self.ha_sync.borrow(), HaSync::Rejected(_) | HaSync::Paused)
            && self
                .live
                .borrow()
                .credentials
                .as_ref()
                .and_then(|credentials| credentials.ha_authorization())
                .is_some_and(|authorization| {
                    authorization.tokens.expires_at > self.clock.unix_now()
                })
    }
    pub(in crate::xiaomi::adapter) fn current_token(&self, token: &str) -> bool {
        self.token_usable()
            && self
                .live
                .borrow()
                .credentials
                .as_ref()
                .and_then(|credentials| credentials.ha_authorization())
                .is_some_and(|authorization| authorization.tokens.access_token == token)
    }
    pub(in crate::xiaomi::adapter) fn mijia_path(&self) -> Option<super::CloudPath> {
        let live = self.live.borrow();
        let credentials = live.credentials.as_ref()?;
        let generation = live.mijia_generation.as_ref()?;
        if self.mijia_rejected.borrow().as_ref() == Some(generation) {
            return None;
        }
        Some(super::CloudPath {
            uid: credentials.uid.clone(),
            generation: generation.clone(),
            home_id: live.scope.as_ref().map(|scope| scope.home_id.clone()),
            session: credentials.mijia.clone()?,
        })
    }
    pub(in crate::xiaomi::adapter) fn current_mijia(
        &self,
        path: &super::CloudPath,
    ) -> Result<bool, AdapterError> {
        Ok(
            self.store.xiaomi().mijia_generation(&path.uid)?.as_deref() == Some(&path.generation)
                && self.mijia_path().is_some_and(|current| {
                    current.uid == path.uid
                        && current.generation == path.generation
                        && (path.home_id.is_none() || current.home_id == path.home_id)
                }),
        )
    }
    pub(in crate::xiaomi::adapter) async fn mijia_retired(
        &self,
        path: &super::CloudPath,
    ) -> Result<(), AdapterError> {
        loop {
            let credentials = self.credentials_changed.listen();
            let changed = self.mijia_changed.listen();
            let result = futures_lite::future::or(
                std::future::poll_fn(|_| match self.current_mijia(path) {
                    Ok(true) => std::task::Poll::Pending,
                    Ok(false) => std::task::Poll::Ready(Some(Ok(()))),
                    Err(error) => std::task::Poll::Ready(Some(Err(error))),
                }),
                async {
                    futures_lite::future::or(credentials, changed).await;
                    None
                },
            )
            .await;
            if let Some(result) = result {
                return result;
            }
        }
    }
    pub(in crate::xiaomi::adapter) fn cloud_error(
        &self,
        path: &super::CloudPath,
        did: Option<&str>,
        error: &CloudError,
    ) -> Result<(), AdapterError> {
        if !self.current_mijia(path)? {
            return Ok(());
        }
        if error.is_unauthorized() {
            *self.mijia_rejected.borrow_mut() = Some(path.generation.clone());
            self.http_paused.set(true);
            self.mijia_authentication(AuthenticationState::Checking);
            self.mijia_changed.notify(usize::MAX);
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
        self.update_routing(&format!("Mijia HTTP request failed: {error}"));
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
    pub(in crate::xiaomi::adapter) fn cloud_property_read_error(
        &self,
        path: &super::CloudPath,
        work: &DeviceWork,
        error: &CloudError,
    ) -> Result<(), AdapterError> {
        if error.is_temporary() || !self.current_mijia(path)? {
            return Ok(());
        }
        let current_work = {
            let live = self.live.borrow();
            !work.life.is_closed()
                && live
                    .devices
                    .get(&work.key.id)
                    .and_then(|slot| slot.admitted.as_deref())
                    .is_some_and(|current| std::ptr::eq(current, work))
        };
        if error.is_unauthorized() {
            self.cloud_error(path, None, error)
        } else if current_work {
            self.cloud_error(path, Some(&work.key.id), error)
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
            .and_then(|record| record.ha_authorization())
            .is_some_and(|authorization| authorization.tokens.access_token == token)
        {
            let mut sync = self.ha_sync.borrow_mut();
            match &*sync {
                HaSync::Refreshed(fresh) if fresh == token => {
                    self.store.xiaomi().stop_ha_refresh()?;
                    *sync = HaSync::Paused;
                    self.status.borrow_mut().auth.ha = AuthenticationState::SignInRequired(reason);
                    self.status_changed.notify(usize::MAX);
                }
                HaSync::Rejected(_) | HaSync::Paused => {}
                _ => {
                    *sync = HaSync::Rejected(token.into());
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
