use super::*;
use crate::{
    device::Availability,
    xiaomi::central::{CentralError, RequestOutcome, RequestStage, RequestTimeout},
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
        self.local_outcome(handle, outcome);
        self.local_device_access_outcome(work, outcome);
    }
    /// A single periodic read timeout says nothing about the shared hub session.
    pub(in crate::xiaomi::adapter) fn local_background_property_outcome<T>(
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
        {
            let mut live = self.live.borrow_mut();
            if let Some(Ok(access)) = &mut live.devices.get_mut(did).unwrap().access {
                access.online = false;
            }
        }
        work.retry_subscriptions();
        self.update_availability(did);
        self.update_routing(&format!("hub reported device {did} offline or missing"));
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn local_outcome<T>(
        &self,
        handle: &SessionHandle,
        outcome: &RequestOutcome<T>,
    ) {
        if outcome.stage != RequestStage::MayHaveBeenSent
            || outcome.timeout != Some(RequestTimeout::Response)
        {
            return;
        }
        let access = self
            .live
            .borrow()
            .session
            .as_ref()
            .filter(|session| session.handle.same_connection(handle))
            .map(|session| session.access.clone());
        if let Some(access) = access
            && access.degrade(self.clock.now())
        {
            self.priority.set(PathPriority::CloudFirst);
            log::warn!(
                "Gateway business response timed out; cloud control now preferred, recovery in 60s"
            );
            handle.cancel_unsent_business();
            for work in self
                .live
                .borrow()
                .devices
                .values()
                .filter_map(|slot| slot.admitted.as_ref())
            {
                work.retry_subscriptions();
            }
            self.update_all_availability();
            self.update_routing("hub business response timed out");
            self.refresh_changed.notify(usize::MAX);
            self.changed.notify(usize::MAX);
        }
    }
    pub(in crate::xiaomi::adapter) fn local_list_success(&self, handle: &SessionHandle) {
        let access = self
            .live
            .borrow()
            .session
            .as_ref()
            .filter(|session| session.handle.same_connection(handle))
            .map(|session| session.access.clone());
        if let Some(access) = access {
            let recovering =
                self.priority.replace(PathPriority::LocalFirst) == PathPriority::CloudFirst;
            access.recovered();
            if recovering {
                log::info!("Gateway device list succeeded; hub control priority restored");
                let works = self
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.admitted.clone())
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
        let mut live = self.live.borrow_mut();
        let work = live.devices.get(did).and_then(|slot| slot.admitted.clone());
        let Some(work) = work else {
            return;
        };
        if let Some(session) = self.notifications.borrow().as_ref() {
            work.attach_cloud(session.handle.clone());
        }
        if !live.devices.get(did).is_some_and(|slot| {
            slot.access.as_ref().is_some_and(|access| {
                access
                    .as_ref()
                    .is_ok_and(|access| access.spec_v2_access && access.push_available)
            })
        }) {
            return;
        }
        let Some(session) = live.session.as_mut() else {
            return;
        };
        let handle = session.handle.clone();
        if !work.attach_local(handle.clone()) {
            return;
        }
        session
            .tasks
            .spawn(super::super::work::run(work, handle, self.clone()));
    }
    pub(in crate::xiaomi::adapter) fn disconnect(&self, reason: String) {
        let priority_changed =
            self.priority.replace(PathPriority::CloudFirst) != PathPriority::CloudFirst;
        let session = self.live.borrow_mut().session.take();
        let disconnected = if let Some(session) = session {
            let works = self
                .live
                .borrow()
                .devices
                .values()
                .filter_map(|slot| slot.admitted.clone())
                .collect::<Vec<_>>();
            for work in works {
                work.detach_local(&session.handle);
            }
            session.handle.close();
            true
        } else {
            false
        };
        for slot in self.live.borrow_mut().devices.values_mut() {
            slot.access = None;
            if (disconnected || priority_changed)
                && let Some(work) = &slot.admitted
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
        if let Some(session) = self.notifications.borrow_mut().take() {
            session.handle.close();
        }
        self.disconnect("Xiaomi adapter stopped".into());
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.stop_lan(&did);
            if let Some(work) = &self.live.borrow().devices[&did].admitted {
                work.physical()
                    .set_availability(Availability::Unavailable("Xiaomi adapter stopped".into()));
                work.retire();
            }
        }
        self.changed.notify(usize::MAX);
    }
}
