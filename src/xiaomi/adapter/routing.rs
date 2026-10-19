use super::{cloud::CloudSync, model::AdapterError, state::Context, work::DeviceWork};
use crate::{
    device::Availability,
    xiaomi::{
        auth::{AuthenticationState, FailureReason},
        central::{CentralError, RequestOutcome, RequestStage, RequestTimeout, SessionHandle},
        cloud::CloudError,
    },
};
use std::{rc::Rc, time::Duration};

pub(super) enum ControlPath {
    Local(SessionHandle),
    Cloud(CloudPath),
}
/// Only the account and token identify an HTTP route; local credentials stay in account state.
pub(super) struct CloudPath {
    pub(super) uid: String,
    pub(super) access_token: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathPriority {
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
    pub(super) fn local_device_outcome<T>(
        &self,
        handle: &SessionHandle,
        work: &DeviceWork,
        outcome: &RequestOutcome<T>,
    ) {
        if work.life.is_closed() || !self.current_local(handle) {
            return;
        }
        self.local_outcome(handle, outcome);
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
        self.update_routing(&format!("local device {did} reported offline or missing"));
        self.refresh_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
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
                .filter_map(|slot| slot.admitted.as_ref())
            {
                work.retry_subscriptions();
            }
            self.update_all_availability();
            self.update_routing("local business response timed out");
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
                    .filter_map(|slot| slot.admitted.clone())
                    .collect::<Vec<_>>();
                for work in works {
                    work.retry_subscriptions();
                    self.refresh_source(&work.key.id);
                }
                self.refresh_changed.notify(usize::MAX);
                self.changed.notify(usize::MAX);
                self.update_routing("current local device list succeeded");
            }
        }
    }
    pub(super) fn install_work(self: &Rc<Self>, did: &str) {
        let mut live = self.live.borrow_mut();
        let slot = live.devices.get_mut(did).unwrap();
        let Some(work) = slot.admitted.clone() else {
            return;
        };
        if !slot.access.as_ref().is_some_and(|access| {
            access.as_ref().is_ok_and(|access| {
                access.model == work.native.model
                    && access.instance_type == work.native.instance_type
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
    pub(super) fn local_candidate(&self, did: &str, push: bool) -> Result<SessionHandle, String> {
        let live = self.live.borrow();
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
        if let Some(reason) = &slot.blocked {
            return Err(reason.clone());
        }
        let session = live.session.as_ref().ok_or("gateway disconnected")?;
        if !self
            .status
            .borrow()
            .authentication
            .certificate
            .is_some_and(|validity| validity.currently_valid(self.clock.unix_now()))
        {
            return Err("gateway certificate not currently valid".into());
        }
        if slot
            .effective_group
            .as_ref()
            .is_some_and(|group| !group.is_physical)
        {
            return Err("virtual group has no local route".into());
        }
        let work = slot
            .admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if work.life.is_closed() {
            return Err("device source retired".into());
        }
        let access = slot
            .access
            .as_ref()
            .ok_or("device access not available")?
            .as_ref()
            .map_err(ToString::to_string)?;
        if !access.online {
            return Err("device offline".into());
        }
        if !access.spec_v2_access {
            return Err("property access unavailable".into());
        }
        if push && !access.push_available {
            return Err("notification access unavailable".into());
        }
        if access.model != work.native.model || access.instance_type != work.native.instance_type {
            return Err("device description or binding unavailable".into());
        }
        Ok(session.handle.clone())
    }
    pub(super) fn is_group(&self, did: &str) -> bool {
        self.live
            .borrow()
            .devices
            .get(did)
            .is_some_and(|slot| slot.effective_group.is_some())
    }
    pub(super) fn cloud_authenticated(&self) -> bool {
        self.cloud_authentication().is_ok()
    }
    fn cloud_authentication(&self) -> Result<(), String> {
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
    pub(super) fn token_usable(&self) -> bool {
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
    pub(super) fn current_token(&self, token: &str) -> bool {
        self.token_usable()
            && self
                .live
                .borrow()
                .credentials
                .as_ref()
                .is_some_and(|credentials| credentials.tokens.access_token == token)
    }
    pub(super) async fn token_retired(&self, token: &str) {
        loop {
            let listener = self.credentials_changed.listen();
            if !self.current_token(token) {
                return;
            }
            listener.await;
        }
    }
    pub(super) fn cloud_candidate(&self, did: &str, http: bool) -> Result<(), String> {
        self.cloud_authentication()?;
        if http && self.http_paused.get() {
            return Err("HTTP temporarily paused".into());
        }
        let live = self.live.borrow();
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
        let work = slot
            .admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if let Some(reason) = &slot.blocked {
            return Err(reason.clone());
        }
        if work.life.is_closed() {
            return Err("device source retired".into());
        }
        if slot.cloud_forbidden {
            return Err("device access forbidden".into());
        }
        let access = slot
            .cloud_access
            .as_ref()
            .ok_or("device information not available")?;
        if !access.online {
            return Err("device offline".into());
        }
        if access.model != work.native.model || access.instance_type != work.native.instance_type {
            return Err("device description or binding unavailable".into());
        }
        Ok(())
    }
    pub(super) fn control_path(&self, did: &str) -> Option<ControlPath> {
        let local = self.local_candidate(did, false).ok();
        match self.priority() {
            PathPriority::LocalFirst if local.is_some() => local.map(ControlPath::Local),
            _ if self.cloud_candidate(did, true).is_ok() => {
                self.live.borrow().credentials.as_ref().map(|credentials| {
                    ControlPath::Cloud(CloudPath {
                        uid: credentials.uid.clone(),
                        access_token: credentials.tokens.access_token.clone(),
                    })
                })
            }
            _ => local.map(ControlPath::Local),
        }
    }
    /// Automatic local reads never bypass the shared slow recovery schedule.
    pub(super) fn read_path(&self, did: &str) -> Option<ControlPath> {
        if self.is_virtual_group(did) {
            return None;
        }
        match self.control_path(did) {
            Some(ControlPath::Local(_)) if self.priority() == PathPriority::CloudFirst => None,
            path => path,
        }
    }
    pub(super) fn group_member_work(&self, did: &str) -> Vec<Rc<DeviceWork>> {
        let live = self.live.borrow();
        let group = live.devices[did].effective_group.as_ref().unwrap();
        group
            .members
            .iter()
            .filter_map(|member| {
                live.devices
                    .get(member)?
                    .admitted
                    .as_ref()
                    .filter(|work| {
                        !work.life.is_closed()
                            && work.native.device.functions().iter().any(|function| {
                                function.id() == "main" && function.lighting().is_some()
                            })
                    })
                    .cloned()
            })
            .collect()
    }
    /// Refresh the actual feedback source, sharing ordinary member work.
    pub(super) fn refresh_source(&self, did: &str) {
        if self.is_virtual_group(did) {
            for work in self.group_member_work(did) {
                work.refresh();
            }
        } else if let Some(work) = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
        {
            work.refresh();
        }
    }
    pub(super) fn update_availability(&self, did: &str) {
        let available = self.control_path(did).is_some();
        let (work, blocked) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (slot.admitted.clone(), slot.blocked.clone())
        };
        let Some(work) = work else {
            return;
        };
        let physical = work.physical();
        let was_available = physical.availability() == Availability::Available;
        physical.set_availability(if available {
            Availability::Available
        } else {
            Availability::Unavailable(blocked.unwrap_or_else(|| "no usable control path".into()))
        });
        if available && !was_available {
            self.refresh_source(did);
        }
        self.refresh_group_states(did);
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
    pub(super) fn disconnect(&self, reason: String) {
        let priority_changed =
            self.priority.replace(PathPriority::CloudFirst) != PathPriority::CloudFirst;
        let disconnected = if let Some(session) = self.live.borrow_mut().session.take() {
            for work in session.devices.values() {
                work.local_disconnected();
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
        self.update_routing(&format!("local gateway unavailable: {reason}"));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn stop(&self) {
        if let Some(session) = self.notifications.borrow_mut().take() {
            session.handle.close();
        }
        self.disconnect("Xiaomi adapter stopped".into());
        for slot in self.live.borrow().devices.values() {
            if let Some(work) = &slot.admitted {
                work.physical()
                    .set_availability(Availability::Unavailable("Xiaomi adapter stopped".into()));
                work.life.close();
                work.current_sink().close();
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
        self.update_routing("cloud access token rejected");
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
