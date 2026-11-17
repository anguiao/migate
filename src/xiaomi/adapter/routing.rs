use super::{state::Context, work::DeviceWork};
use crate::xiaomi::{capability::ReadInput, central::SessionHandle, lan::Client};
use std::rc::Rc;

mod availability;
mod cloud;
mod evaluation;
mod history;
mod hub;

pub(super) use evaluation::{DeviceRoutes, Selection, Summaries};
pub(super) use history::{DeviceHistory, PriorityHistory};

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
    pub(super) fn kind(&self) -> super::model::PathKind {
        match self {
            Self::Local(_) => super::model::PathKind::Local,
            Self::Direct(_) => super::model::PathKind::Direct,
            Self::Cloud(_) => super::model::PathKind::Cloud,
        }
    }
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
impl DeviceRoutes {
    pub(super) fn read_path_for(&self, id: &str) -> Option<&ControlPath> {
        self.functions.get(id)?.read.selected.as_ref()
    }
    pub(super) fn read_path_for_input(&self, id: &str, input: ReadInput) -> Option<&ControlPath> {
        match input {
            ReadInput::Property(_) => self.read_path_for(id),
            ReadInput::SharedFeedback => self.shared_feedback.as_ref()?.selected.as_ref(),
        }
    }
}
impl Context {
    pub(super) fn current_local(&self, handle: &SessionHandle) -> bool {
        self.hub
            .borrow()
            .session()
            .is_some_and(|session| session.handle.same_connection(handle))
    }
    pub(super) fn local_function_reads_allowed(
        &self,
        did: &str,
        id: &str,
        handle: &SessionHandle,
    ) -> bool {
        matches!(self.read_path_for(did, id), Some(ControlPath::Local(selected)) if selected.same_connection(handle))
    }
    pub(super) fn local_candidate(&self, did: &str, push: bool) -> Result<SessionHandle, String> {
        let routes = self.device_routes(did);
        if push {
            routes.hub_push
        } else {
            routes.read.candidates.hub
        }
    }
    pub(super) fn is_group(&self, did: &str) -> bool {
        self.directory.borrow().devices.contains_key(did)
            && self.graph.borrow().rules.contains_key(did)
    }
    pub(super) fn cloud_observation_candidate(&self, did: &str, http: bool) -> Result<(), String> {
        let routes = self.device_routes(did);
        routes.cloud_candidate(
            http,
            routes.functions.values().any(|function| function.read_only),
        )
    }
    pub(super) fn control_path_for(&self, did: &str, id: &str) -> Option<ControlPath> {
        self.device_routes(did)
            .functions
            .get(id)?
            .control
            .selected
            .clone()
    }
    pub(super) fn low_speed_path(&self, did: &str, id: &str) -> Option<ControlPath> {
        self.device_routes(did)
            .functions
            .get(id)?
            .low_speed
            .as_ref()?
            .selected
            .clone()
    }
    pub(super) fn read_path_for_input(
        &self,
        did: &str,
        id: &str,
        input: ReadInput,
    ) -> Option<ControlPath> {
        self.device_routes(did)
            .read_path_for_input(id, input)
            .cloned()
    }
    pub(super) fn read_path_for(&self, did: &str, id: &str) -> Option<ControlPath> {
        self.device_routes(did).read_path_for(id).cloned()
    }
}
