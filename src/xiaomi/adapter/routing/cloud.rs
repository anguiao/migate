use super::*;
use crate::xiaomi::cloud::CloudError;
use std::time::Duration;

impl Context {
    pub(in crate::xiaomi::adapter) fn cloud_authenticated(&self) -> bool {
        self.cloud_authentication().is_ok()
    }
    pub(in crate::xiaomi::adapter) fn cloud_authentication(&self) -> Result<(), String> {
        self.cloud_authentication_at(self.clock.unix_now())
    }
    pub(in crate::xiaomi::adapter) fn ha_key(&self) -> Option<super::HaKey> {
        self.credentials.borrow().ha_key()
    }
    pub(in crate::xiaomi::adapter) fn current_ha(&self, key: &super::HaKey) -> bool {
        self.credentials.borrow().current_ha(key)
            && self.store.xiaomi().current_ha(&key.uid, key.generation)
    }
    pub(in crate::xiaomi::adapter) fn token_usable(&self) -> bool {
        self.credentials
            .borrow()
            .token_usable(self.clock.unix_now())
    }
    pub(in crate::xiaomi::adapter) fn mijia_path(&self) -> Option<super::CloudPath> {
        let home_id = self
            .directory
            .borrow()
            .scope
            .as_ref()
            .map(|scope| scope.home_id.clone());
        self.credentials.borrow().mijia_path(home_id)
    }
    pub(in crate::xiaomi::adapter) fn current_mijia(&self, path: &super::CloudPath) -> bool {
        self.credentials.borrow().current_mijia(path)
            && self
                .store
                .xiaomi()
                .current_mijia(&path.uid, path.generation)
            && path.home_id.as_ref().is_none_or(|home_id| {
                self.directory
                    .borrow()
                    .scope
                    .as_ref()
                    .is_some_and(|scope| scope.home_id == *home_id)
            })
    }
    pub(in crate::xiaomi::adapter) async fn mijia_retired(&self, path: &super::CloudPath) {
        loop {
            let credentials = self.credentials_changed.listen();
            let changed = self.mijia_changed.listen();
            if !self.current_mijia(path) {
                return;
            }
            futures_lite::future::or(credentials, changed).await;
        }
    }
    pub(in crate::xiaomi::adapter) fn cloud_error(
        &self,
        path: &super::CloudPath,
        did: Option<&str>,
        error: &CloudError,
    ) {
        if !self.current_mijia(path) {
            return;
        }
        if error.is_unauthorized() {
            self.credentials
                .borrow_mut()
                .reject_mijia(path.generation, self.clock.unix_now());
            self.status_changed.notify(usize::MAX);
            self.mijia_changed.notify(usize::MAX);
        } else if error.is_temporary() {
            let newly_paused = self.credentials.borrow_mut().pause_http();
            if newly_paused {
                self.directory.borrow_mut().sync_failed(self.clock.now());
                self.request_sync();
            }
        } else if let Some(did) = did {
            self.directory.borrow_mut().device_error(did, error);
        }
        self.update_all_availability();
        self.update_routing(&format!("Mijia HTTP request failed: {error}"));
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn cloud_property_read_error(
        &self,
        path: &super::CloudPath,
        work: &DeviceWork,
        error: &CloudError,
    ) {
        if error.is_temporary() || !self.current_mijia(path) {
            return;
        }
        let current_work = {
            let live = self.directory.borrow();
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
        }
    }
    pub(in crate::xiaomi::adapter) fn sync_failed(&self) -> Duration {
        self.directory.borrow_mut().sync_failed(self.clock.now())
    }
}
