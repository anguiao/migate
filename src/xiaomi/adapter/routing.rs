use super::{cloud::CloudSync, model::AdapterError, state::Context, work::DeviceWork};
use crate::{
    device::Availability,
    storage::XiaomiRecord,
    xiaomi::{
        auth::{AuthenticationState, FailureReason},
        central::SessionHandle,
        cloud::CloudError,
    },
};
use std::{rc::Rc, time::Duration};

pub(super) enum ControlPath {
    Local(SessionHandle),
    Cloud(XiaomiRecord),
}
impl Context {
    pub(super) fn install_work(self: &Rc<Self>, did: &str) {
        let mut live = self.live.borrow_mut();
        let slot = live.devices.get_mut(did).unwrap();
        let record = slot.admitted.as_ref().unwrap().clone();
        let work = slot
            .work
            .get_or_insert_with(|| Rc::new(DeviceWork::new(record, self.refresh_changed.clone())))
            .clone();
        if !slot.access.as_ref().is_some_and(|access| {
            access.as_ref().is_ok_and(|access| {
                let cached = work.record.cached.borrow();
                access.model == cached.model
                    && access.instance_type == cached.instance_type
                    && access.spec_v2_access
                    && access.push_available
            })
        }) {
            return;
        }
        let Some(session) = live.session.as_mut() else {
            return;
        };
        if session.devices.contains_key(did) {
            return;
        }
        work.local_connected();
        let handle = session.handle.clone();
        session.devices.insert(did.into(), work.clone());
        session
            .tasks
            .spawn(super::work::run(work, handle, self.clone()));
    }
    pub(super) fn control_path(&self, did: &str) -> Option<ControlPath> {
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let record = slot.admitted.as_ref()?;
        if slot.blocked.is_some() || record.life.is_closed() {
            return None;
        }
        let cached = record.cached.borrow();
        if let Some(session) = &live.session
            && slot.access.as_ref().is_some_and(|access| {
                access.as_ref().is_ok_and(|access| {
                    access.online
                        && access.spec_v2_access
                        && access.model == cached.model
                        && access.instance_type == cached.instance_type
                })
            })
        {
            return Some(ControlPath::Local(session.handle.clone()));
        }
        if self.cloud_scope_valid.get()
            && !self.http_paused.get()
            && !matches!(
                *self.cloud_sync.borrow(),
                CloudSync::Rejected(_) | CloudSync::Paused
            )
            && !slot.cloud_forbidden
            && slot.cloud_access.as_ref().is_some_and(|access| {
                access.online
                    && access.model == cached.model
                    && access.instance_type == cached.instance_type
            })
        {
            return live.credentials.clone().map(ControlPath::Cloud);
        }
        None
    }
    pub(super) fn update_availability(&self, did: &str) {
        let available = self.control_path(did).is_some();
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let Some(record) = &slot.admitted else {
            return;
        };
        let was_available = record.physical.availability() == Availability::Available;
        record.physical.set_availability(if available {
            Availability::Available
        } else {
            Availability::Unavailable(
                slot.blocked
                    .clone()
                    .unwrap_or_else(|| "no usable control path".into()),
            )
        });
        if available
            && !was_available
            && let Some(work) = &slot.work
        {
            work.refresh();
        }
    }
    pub(super) fn update_all_availability(&self) {
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.update_availability(&did);
        }
    }
    pub(super) fn disconnect(&self, _reason: String) {
        if let Some(session) = self.live.borrow_mut().session.take() {
            for work in session.devices.values() {
                work.local_disconnected();
            }
            session.handle.close();
        }
        for slot in self.live.borrow_mut().devices.values_mut() {
            slot.access = None;
        }
        self.set_access_problem(None);
        self.update_all_availability();
        self.changed.notify(usize::MAX);
    }
    pub(super) fn stop(&self) {
        self.disconnect("Xiaomi adapter stopped".into());
        for slot in self.live.borrow().devices.values() {
            if let Some(record) = &slot.admitted {
                record.life.close();
                record
                    .physical
                    .set_availability(Availability::Unavailable("Xiaomi adapter stopped".into()));
            }
            if let Some(work) = &slot.work {
                work.life.close();
            }
        }
    }
    /// Token rejection and HTTP failure share one account maintenance entry point.
    pub(super) fn cloud_error(
        &self,
        token: &str,
        did: Option<&str>,
        error: &CloudError,
    ) -> Result<(), AdapterError> {
        if error.is_unauthorized() {
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
                            AuthenticationState::SignInRequired(FailureReason::Cloud(
                                error.clone(),
                            ));
                        self.status_changed.notify(usize::MAX);
                    }
                    CloudSync::Rejected(_) | CloudSync::Paused => {}
                    _ => {
                        *sync = CloudSync::Rejected(token.into());
                        self.request_sync();
                    }
                }
            }
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
        Ok(())
    }
    pub(super) fn sync_failed(&self) -> Duration {
        let due = self.clock.now() + self.sync_backoff.borrow_mut().failed();
        self.sync_not_before.set(Some(due));
        due
    }
}
