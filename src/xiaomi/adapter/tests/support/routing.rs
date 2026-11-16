//! Convenience projections of the production routing evaluation.

use super::super::super::{
    model::PathKind,
    routing::{ControlPath, PathPriority},
    state::Context,
};
use crate::xiaomi::lan::Client;

impl Context {
    pub(in crate::xiaomi::adapter::tests) fn priority(&self) -> PathPriority {
        self.hub.borrow().priority()
    }
    pub(in crate::xiaomi::adapter::tests) fn direct_candidate(
        &self,
        did: &str,
    ) -> Result<Client, String> {
        self.device_routes(did).read.candidates.direct.clone()
    }
    pub(in crate::xiaomi::adapter::tests) fn cloud_candidate(
        &self,
        did: &str,
        http: bool,
    ) -> Result<(), String> {
        let routes = self.device_routes(did);
        routes.cloud_candidate(http, routes.read_only)
    }
    pub(in crate::xiaomi::adapter::tests) fn control_path(&self, did: &str) -> Option<ControlPath> {
        self.device_routes(did).control.selected.clone()
    }
    /// Automatic reads never fall back to a degraded hub.
    pub(in crate::xiaomi::adapter::tests) fn read_path(&self, did: &str) -> Option<ControlPath> {
        self.device_routes(did).read.selected.clone()
    }
    pub(in crate::xiaomi::adapter::tests) fn active_notification(
        &self,
        did: &str,
    ) -> Option<PathKind> {
        self.device_routes(did).notification_source
    }
}
