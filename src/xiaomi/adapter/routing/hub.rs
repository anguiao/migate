use super::*;
use crate::{
    device::Availability,
    xiaomi::central::{CentralError, RequestOutcome, RequestStage},
};

impl Context {
    pub(in crate::xiaomi::adapter) fn local_device_outcome<T>(
        &self,
        handle: &SessionHandle,
        work: &DeviceWork,
        outcome: &RequestOutcome<T>,
    ) {
        if work.life.is_closed() || !self.current_local(handle) {
            return;
        }
        self.local_device_access_outcome(work, outcome);
    }
    pub(super) fn local_device_access_outcome<T>(
        &self,
        work: &DeviceWork,
        outcome: &RequestOutcome<T>,
    ) {
        // Xiaomi's device offline/missing errors revoke this device, not the shared gateway.
        if outcome.stage != RequestStage::BusinessResult
            || !matches!(
                outcome.result,
                Err(CentralError::Rejected(
                    -10_007 | -704_042_001 | -704_042_011 | -704_090_001
                ))
            )
        {
            return;
        }
        let did = &work.key.id;
        self.hub.borrow_mut().device_offline(did);
        work.retry_subscriptions();
        self.update_availability(did);
        self.update_device_routing(
            did,
            &format!("hub reported device {did} offline or missing"),
        );
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn local_list_success(&self, handle: &SessionHandle) {
        if self.current_local(handle) {
            let recovering = self.hub.borrow_mut().restore_priority();
            if recovering {
                log::info!("Gateway device list succeeded; hub control priority restored");
                let works = self
                    .directory
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.work.clone())
                    .collect::<Vec<_>>();
                for work in works {
                    work.retry_subscriptions();
                    self.refresh_source(&work.key.id);
                }
                self.refresh_changed.notify(usize::MAX);
                self.changed.notify(usize::MAX);
                self.update_routing("current hub device list succeeded");
            }
        }
    }
    pub(in crate::xiaomi::adapter) fn install_work(self: &Rc<Self>, did: &str) {
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let Some(work) = work else {
            return;
        };
        if let Some(session) = self.notifications.borrow().as_ref() {
            work.attach_cloud(session.handle.clone());
        }
        let hub = self.hub.borrow();
        if !hub.access(did).is_some_and(|access| {
            access
                .as_ref()
                .is_ok_and(|access| access.spec_v2_access && access.push_available)
        }) {
            return;
        }
        let Some(session) = hub.session() else {
            return;
        };
        let handle = session.handle.clone();
        let tasks = session.tasks.clone();
        drop(hub);
        if !work.attach_local(handle.clone()) {
            return;
        }
        tasks.spawn(super::super::work::run(work, handle, self.clone()));
    }
    pub(in crate::xiaomi::adapter) fn disconnect(&self, reason: String) {
        let (session, priority_changed) = self.hub.borrow_mut().disconnect();
        let disconnected = if let Some(session) = session {
            let works = self
                .directory
                .borrow()
                .devices
                .values()
                .filter_map(|slot| slot.work.clone())
                .collect::<Vec<_>>();
            for work in works {
                work.detach_local(&session.handle);
            }
            session.handle.close();
            true
        } else {
            false
        };
        for slot in self.directory.borrow().devices.values() {
            if (disconnected || priority_changed)
                && let Some(work) = &slot.work
            {
                work.retry_subscriptions();
            }
        }
        self.set_access_problem(None);
        self.update_all_availability();
        self.update_routing(&format!("hub unavailable: {reason}"));
        self.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn stop(&self) {
        if self.lifetime.is_closed() {
            return;
        }
        self.lifetime.close();
        let notifications = self.notifications.borrow_mut().take();
        if let Some(session) = notifications {
            session.handle.close();
        }
        self.disconnect("Xiaomi adapter stopped".into());
        let retired = self
            .directory
            .replace(super::super::directory::Directory::restored(
                None,
                Default::default(),
                Vec::new(),
            ));
        for slot in retired.devices.into_values() {
            if let Some(loading) = slot.loading {
                loading.life.close();
            }
            if let Some(work) = slot.work {
                work.physical()
                    .set_availability(Availability::Unavailable("Xiaomi adapter stopped".into()));
                work.retire();
            }
        }
        self.configuration_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }
}
