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
            (Self::Cloud(a), Self::Cloud(b)) => a.uid == b.uid && a.access_token == b.access_token,
            _ => false,
        }
    }
}
/// Only the account and token identify an HTTP route; local credentials stay in account state.
#[derive(Clone)]
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
    pub(super) fn direct_candidate(&self, did: &str) -> Result<Client, String> {
        let live = self.live.borrow();
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
        let live = self.live.borrow();
        let slot = live
            .devices
            .get(did)
            .ok_or("device description unavailable")?;
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
        Ok(session.handle.clone())
    }
    pub(super) fn is_group(&self, did: &str) -> bool {
        self.live.borrow().devices.contains_key(did) && self.graph.borrow().rules.contains_key(did)
    }
    pub(super) fn is_read_only(&self, did: &str) -> bool {
        let live = self.live.borrow();
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
        self.live
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
        self.live
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
            let live = self.live.borrow();
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
        if self.has_sensors(did) {
            return self.observation_path(did);
        }
        self.control_read_path(did)
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
