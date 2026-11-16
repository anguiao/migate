use super::{state::Context, work::DeviceWork};
use crate::xiaomi::{central::SessionHandle, lan::Client};
use std::rc::Rc;

mod availability;
mod cloud;
mod hub;

/// Official Xiaomi source omits state subscriptions for BLE and proxy-child
/// identities; their cloud online flag is not a sensor eligibility signal.
pub(super) fn unreliable_cloud_online(did: &str) -> bool {
    did.starts_with("blt.") || did.starts_with("proxy.")
}

#[derive(Clone)]
pub(super) enum ControlPath {
    Local(SessionHandle),
    Direct(Client),
    Cloud(CloudPath),
}
impl ControlPath {
    pub(super) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => a.same_connection(b),
            (Self::Direct(a), Self::Direct(b)) => a.same(b),
            (Self::Cloud(a), Self::Cloud(b)) => {
                a.uid == b.uid && a.generation == b.generation && a.home_id == b.home_id
            }
            _ => false,
        }
    }
}
/// A captured device session and its committed process-local credential identity.
#[derive(Clone)]
pub(super) struct CloudPath {
    pub(super) uid: String,
    pub(super) generation: u64,
    pub(super) home_id: Option<String>,
    pub(super) session: crate::xiaomi::mijia::MijiaSession,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HaKey {
    pub(super) uid: String,
    pub(super) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathPriority {
    LocalFirst,
    CloudFirst,
}
impl Context {
    pub(super) fn priority(&self) -> PathPriority {
        self.hub.borrow().priority()
    }
    pub(super) fn current_local(&self, handle: &SessionHandle) -> bool {
        self.hub
            .borrow()
            .session()
            .is_some_and(|session| session.handle.same_connection(handle))
    }
    pub(super) fn direct_candidate(&self, did: &str) -> Result<Client, String> {
        let live = self.directory.borrow();
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
        let work = slot
            .admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if self.lan_scope_invalid.get() || work.life.is_closed() {
            return Err("device source retired or scope revoked".into());
        }
        if slot
            .committed
            .as_ref()
            .and_then(|source| source.native.as_ref())
            .is_none_or(|native| super::lan::protocol(native).is_none())
        {
            return Err("device model not supported".into());
        }
        slot.lan
            .active
            .as_ref()
            .map(|(_, client)| client.clone())
            .ok_or_else(|| {
                slot.lan.status.as_ref().map_or_else(
                    || "not yet initialized".into(),
                    |status| {
                        status.problem.clone().unwrap_or_else(|| {
                            match status.state {
                                super::model::LanState::Ineligible => {
                                    "device is not an eligible IP device"
                                }
                                super::model::LanState::MissingCredentials => "credentials missing",
                                super::model::LanState::InvalidCredentials => "credentials invalid",
                                super::model::LanState::Authenticating => {
                                    "authentication in progress"
                                }
                                super::model::LanState::Ready => "session unavailable",
                                super::model::LanState::Failed => {
                                    "authentication or communication failed"
                                }
                            }
                            .into()
                        })
                    },
                )
            })
    }
    pub(super) fn local_function_reads_allowed(
        &self,
        did: &str,
        id: &str,
        handle: &SessionHandle,
    ) -> bool {
        self.priority() == PathPriority::LocalFirst
            && matches!(self.read_path_for(did, id), Some(ControlPath::Local(selected)) if selected.same_connection(handle))
    }
    pub(super) fn local_candidate(&self, did: &str, push: bool) -> Result<SessionHandle, String> {
        let virtual_group = self.is_virtual_group(did);
        let live = self.directory.borrow();
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
        let hub = self.hub.borrow();
        let session = hub.session().ok_or("gateway disconnected")?;
        if !self.credentials.borrow().hub_usable(self.clock.unix_now()) {
            return Err("gateway certificate not currently valid".into());
        }
        if virtual_group {
            return Err("virtual group has no hub route".into());
        }
        let work = slot
            .admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if work.life.is_closed() {
            return Err("device source retired".into());
        }
        let access = hub
            .access(did)
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
        Ok(session.handle.clone())
    }
    pub(super) fn is_group(&self, did: &str) -> bool {
        self.directory.borrow().devices.contains_key(did)
            && self.graph.borrow().rules.contains_key(did)
    }
    pub(super) fn is_read_only(&self, did: &str) -> bool {
        let live = self.directory.borrow();
        let Some(slot) = live.devices.get(did) else {
            return false;
        };
        slot.admitted
            .as_ref()
            .map(|work| &work.native)
            .or_else(|| {
                slot.candidate
                    .as_ref()
                    .and_then(|source| source.native.as_ref())
            })
            .is_some_and(|native| native.device().is_read_only())
    }
    pub(super) fn cloud_candidate(&self, did: &str, http: bool) -> Result<(), String> {
        self.cloud_candidate_for(did, http, self.is_read_only(did))
    }
    pub(super) fn function_cloud_candidate(
        &self,
        did: &str,
        id: &str,
        http: bool,
    ) -> Result<(), String> {
        self.cloud_candidate_for(did, http, self.sensor_function(did, id))
    }
    pub(super) fn cloud_observation_candidate(&self, did: &str, http: bool) -> Result<(), String> {
        self.cloud_candidate_for(did, http, self.has_sensors(did))
    }
    pub(super) fn has_sensors(&self, did: &str) -> bool {
        self.directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
            .is_some_and(|work| {
                work.native
                    .device()
                    .functions()
                    .iter()
                    .any(|function| function.kind().is_read_only())
            })
    }
    fn sensor_function(&self, did: &str, id: &str) -> bool {
        self.directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
            .is_some_and(|work| {
                if id == "battery" {
                    return work.native.device().battery().is_some();
                }
                work.native
                    .device()
                    .functions()
                    .iter()
                    .any(|function| function.id() == id && function.kind().is_read_only())
            })
    }
    fn cloud_candidate_for(&self, did: &str, http: bool, sensor: bool) -> Result<(), String> {
        if http {
            if !self.directory.borrow().scope_valid() {
                return Err("account/home scope not validated".into());
            }
            if self.credentials.borrow().http_paused() || self.mijia_path().is_none() {
                return Err("Mijia HTTP unavailable".into());
            }
        } else {
            self.cloud_authentication()?;
        }
        let live = self.directory.borrow();
        if !self
            .credentials
            .borrow()
            .record()
            .is_some_and(|credentials| {
                live.scope
                    .as_ref()
                    .is_some_and(|scope| scope.uid == credentials.uid)
            })
        {
            return Err("account/home identity mismatch".into());
        }
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
        let work = slot
            .admitted
            .as_ref()
            .ok_or("device description or binding unavailable")?;
        if work.life.is_closed() {
            return Err("device source retired".into());
        }
        if slot.cloud_forbidden {
            return Err("device access forbidden".into());
        }
        let Some(access) = slot.cloud_access.as_ref() else {
            return if !http
                && (self.mijia_path().is_none() || self.credentials.borrow().http_paused())
            {
                Ok(())
            } else {
                Err("device information not available".into())
            };
        };
        // BLE and proxy-child devices have no reliable cloud state signal.
        // Account, membership, authorization, retirement and explicitly
        // forbidden access still decide here; ordinary devices stay offline.
        if !(access.online || sensor && unreliable_cloud_online(did)) {
            return Err("device offline".into());
        }
        Ok(())
    }
    /// Read-only eligibility is independent of a native write path.
    pub(super) fn observation_path(&self, did: &str) -> Option<ControlPath> {
        if self.is_virtual_group(did) {
            return None;
        }
        if !self.has_sensors(did) {
            return self.control_path(did);
        }
        self.observation_path_with_direct(did, self.direct_candidate(did).ok())
    }
    fn observation_path_with_direct(
        &self,
        did: &str,
        direct: Option<Client>,
    ) -> Option<ControlPath> {
        let local = self.local_candidate(did, false).ok();
        match self.priority() {
            PathPriority::LocalFirst if local.is_some() => local.map(ControlPath::Local),
            _ if direct.is_some() => direct.map(ControlPath::Direct),
            _ if self.cloud_observation_candidate(did, true).is_ok() => {
                self.mijia_path().map(ControlPath::Cloud)
            }
            _ => local.map(ControlPath::Local),
        }
    }
    pub(super) fn control_path(&self, did: &str) -> Option<ControlPath> {
        if self.is_read_only(did) {
            return None;
        }
        self.control_path_with_direct(did, self.direct_candidate(did).ok())
    }
    pub(super) fn control_path_for(&self, did: &str, id: &str) -> Option<ControlPath> {
        if self.is_read_only(did) {
            return None;
        }
        self.control_path_with_direct(did, self.direct_function_candidate(did, id))
    }
    fn direct_function_candidate(&self, did: &str, id: &str) -> Option<Client> {
        {
            let live = self.directory.borrow();
            let native = live.devices.get(did)?.committed.as_ref()?.native.as_ref()?;
            if native
                .legacy_profile()
                .is_some_and(|profile| profile.target() != id)
            {
                return None;
            }
        }
        self.direct_candidate(did).ok()
    }
    fn control_path_with_direct(&self, did: &str, direct: Option<Client>) -> Option<ControlPath> {
        let local = self.local_candidate(did, false).ok();
        match self.priority() {
            PathPriority::LocalFirst if local.is_some() => local.map(ControlPath::Local),
            _ if direct.is_some() => direct.map(ControlPath::Direct),
            _ if self.cloud_candidate(did, true).is_ok() => {
                self.mijia_path().map(ControlPath::Cloud)
            }
            _ => local.map(ControlPath::Local),
        }
    }
    /// Automatic local reads wait for the current hub's directory to recover.
    pub(super) fn read_path(&self, did: &str) -> Option<ControlPath> {
        if self.is_virtual_group(did) {
            return None;
        }
        if self.has_sensors(did) {
            return self.observation_path(did);
        }
        self.control_read_path(did)
    }
    /// Low speed is only executable by the direct legacy command or cloud
    /// action. The hub is never a candidate, and high/off stay standard writes.
    pub(super) fn low_speed_path(&self, did: &str, id: &str) -> Option<ControlPath> {
        let candidates = self.low_speed_candidate_paths(did, id);
        if candidates.contains(&super::model::PathKind::Direct)
            && let Ok(client) = self.direct_candidate(did)
        {
            return Some(ControlPath::Direct(client));
        }
        if candidates.contains(&super::model::PathKind::Cloud) {
            return self.mijia_path().map(ControlPath::Cloud);
        }
        None
    }
    pub(super) fn low_speed_candidate_paths(
        &self,
        did: &str,
        id: &str,
    ) -> Vec<super::model::PathKind> {
        let Some(has_cloud_action) = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
            .and_then(|work| {
                work.native
                    .device()
                    .functions()
                    .iter()
                    .find(|function| function.id() == id)
            })
            .and_then(|function| function.fan())
            .and_then(|fan| fan.shared_speed())
            .map(|speed| speed.cloud_action.is_some())
        else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        if self.direct_candidate(did).is_ok() {
            paths.push(super::model::PathKind::Direct);
        }
        if has_cloud_action
            && self.cloud_candidate(did, true).is_ok()
            && self.mijia_path().is_some()
        {
            paths.push(super::model::PathKind::Cloud);
        }
        paths
    }
    /// Shared legacy feedback is a physical-device read owned by direct/cloud,
    /// never a hub standard-property read.
    pub(super) fn shared_feedback_candidate_paths(&self, did: &str) -> Vec<super::model::PathKind> {
        if self.is_virtual_group(did) {
            return Vec::new();
        }
        let has_plan = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
            .is_some_and(|work| work.native.plan().shared_feedback().is_some());
        if !has_plan {
            return Vec::new();
        }
        let mut paths = Vec::new();
        if self.direct_candidate(did).is_ok() {
            paths.push(super::model::PathKind::Direct);
        }
        if self.cloud_candidate(did, true).is_ok() && self.mijia_path().is_some() {
            paths.push(super::model::PathKind::Cloud);
        }
        paths
    }
    pub(super) fn shared_feedback_read_path(&self, did: &str) -> Option<ControlPath> {
        let paths = self.shared_feedback_candidate_paths(did);
        if paths.contains(&super::model::PathKind::Direct)
            && let Ok(client) = self.direct_candidate(did)
        {
            return Some(ControlPath::Direct(client));
        }
        if paths.contains(&super::model::PathKind::Cloud) {
            return self.mijia_path().map(ControlPath::Cloud);
        }
        None
    }
    pub(super) fn read_path_for_input(
        &self,
        did: &str,
        id: &str,
        input: crate::xiaomi::capability::ReadInput,
    ) -> Option<ControlPath> {
        match input {
            crate::xiaomi::capability::ReadInput::Property(_) => self.read_path_for(did, id),
            crate::xiaomi::capability::ReadInput::SharedFeedback => {
                self.shared_feedback_read_path(did)
            }
        }
    }
    pub(super) fn read_path_for(&self, did: &str, id: &str) -> Option<ControlPath> {
        if self.is_virtual_group(did) {
            return None;
        }
        if self.sensor_function(did, id) {
            return self.observation_path_with_direct(did, self.direct_function_candidate(did, id));
        }
        match self.control_path_for(did, id) {
            Some(ControlPath::Local(_)) if self.priority() == PathPriority::CloudFirst => None,
            path => path,
        }
    }
    fn control_read_path(&self, did: &str) -> Option<ControlPath> {
        if self.is_virtual_group(did) {
            return None;
        }
        match self.control_path(did) {
            Some(ControlPath::Local(_)) if self.priority() == PathPriority::CloudFirst => None,
            path => path,
        }
    }
}
