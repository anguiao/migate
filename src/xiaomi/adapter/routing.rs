use super::{cloud::CloudSync, model::AdapterError, state::Context, work::DeviceWork};
use crate::{
    device::Availability,
    storage::XiaomiRecord,
    xiaomi::{
        auth::{AuthenticationState, FailureReason},
        central::{RequestOutcome, RequestStage, RequestTimeout, SessionHandle},
        cloud::CloudError,
    },
};
use std::{rc::Rc, time::Duration};

pub(super) enum ControlPath {
    Local(SessionHandle),
    Cloud(XiaomiRecord),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PathPriority {
    LocalFirst,
    CloudFirst,
}
impl Context {
    pub(super) fn priority(&self) -> PathPriority {
        self.priority.get()
    }
    pub(super) fn current_local(&self, handle: &SessionHandle) -> bool {
        self.live
            .borrow()
            .session
            .as_ref()
            .is_some_and(|session| session.handle.same_connection(handle))
    }
    pub(super) fn local_reads_allowed(&self, did: &str, handle: &SessionHandle) -> bool {
        self.priority() == PathPriority::LocalFirst
            && matches!(self.read_path(did), Some(ControlPath::Local(selected)) if selected.same_connection(handle))
    }
    pub(super) fn local_outcome<T>(&self, handle: &SessionHandle, outcome: &RequestOutcome<T>) {
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
                .filter_map(|slot| slot.work.as_ref())
            {
                work.retry_subscriptions();
            }
            self.update_all_availability();
            self.refresh_changed.notify(usize::MAX);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn local_list_success(&self, handle: &SessionHandle) {
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
                log::info!("Gateway device list succeeded; local control priority restored");
                let works = self
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.work.clone())
                    .collect::<Vec<_>>();
                for work in works {
                    work.retry_subscriptions();
                    work.refresh();
                }
                self.refresh_changed.notify(usize::MAX);
                self.changed.notify(usize::MAX);
            }
        }
    }
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
    pub(super) fn local_candidate(&self, did: &str, push: bool) -> Option<SessionHandle> {
        let live = self.live.borrow();
        let slot = live.devices.get(did)?;
        let record = slot.admitted.as_ref()?;
        if slot.blocked.is_some() || record.life.is_closed() {
            return None;
        }
        let cached = record.cached.borrow();
        let access = slot.access.as_ref()?.as_ref().ok()?;
        if !access.online
            || !access.spec_v2_access
            || (push && !access.push_available)
            || access.model != cached.model
            || access.instance_type != cached.instance_type
        {
            return None;
        }
        live.session.as_ref().map(|session| session.handle.clone())
    }
    pub(super) fn cloud_authenticated(&self) -> bool {
        self.cloud_scope_valid.get()
            && !matches!(
                *self.cloud_sync.borrow(),
                CloudSync::Rejected(_) | CloudSync::Paused
            )
            && {
                let live = self.live.borrow();
                live.credentials.as_ref().is_some_and(|credentials| {
                    live.scope
                        .as_ref()
                        .is_some_and(|scope| scope.uid == credentials.uid)
                })
            }
    }
    pub(super) fn cloud_candidate(&self, did: &str, http: bool) -> bool {
        if !self.cloud_authenticated() || (http && self.http_paused.get()) {
            return false;
        }
        let live = self.live.borrow();
        let Some(slot) = live.devices.get(did) else {
            return false;
        };
        let Some(record) = &slot.admitted else {
            return false;
        };
        if slot.blocked.is_some() || record.life.is_closed() || slot.cloud_forbidden {
            return false;
        }
        let cached = record.cached.borrow();
        slot.cloud_access.as_ref().is_some_and(|access| {
            access.online
                && access.model == cached.model
                && access.instance_type == cached.instance_type
        })
    }
    pub(super) fn control_path(&self, did: &str) -> Option<ControlPath> {
        let local = self.local_candidate(did, false);
        match self.priority() {
            PathPriority::LocalFirst if local.is_some() => local.map(ControlPath::Local),
            _ if self.cloud_candidate(did, true) => self
                .live
                .borrow()
                .credentials
                .clone()
                .map(ControlPath::Cloud),
            _ => local.map(ControlPath::Local),
        }
    }
    /// Automatic local reads never bypass the shared slow recovery schedule.
    pub(super) fn read_path(&self, did: &str) -> Option<ControlPath> {
        match self.control_path(did) {
            Some(ControlPath::Local(_)) if self.priority() == PathPriority::CloudFirst => None,
            path => path,
        }
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
        let priority_changed =
            self.priority.replace(PathPriority::CloudFirst) != PathPriority::CloudFirst;
        if let Some(session) = self.live.borrow_mut().session.take() {
            for work in session.devices.values() {
                work.local_disconnected();
            }
            session.handle.close();
        }
        for slot in self.live.borrow_mut().devices.values_mut() {
            slot.access = None;
            if priority_changed && let Some(work) = &slot.work {
                work.retry_subscriptions();
            }
        }
        self.set_access_problem(None);
        self.update_all_availability();
        self.changed.notify(usize::MAX);
    }
    pub(super) fn stop(&self) {
        if let Some(session) = self.notifications.borrow_mut().take() {
            session.handle.close();
        }
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
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
    pub(super) fn token_rejected(
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
        self.changed.notify(usize::MAX);
        self.credentials_changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
        Ok(())
    }
    pub(super) fn sync_failed(&self) -> Duration {
        let due = self.clock.now() + self.sync_backoff.borrow_mut().failed();
        self.sync_not_before.set(Some(due));
        due
    }
}
