//! Shared priority selection for control, reads and subscription preparation.

use super::{Client, CloudPath, ControlPath, PathKind, PathPriority, SessionHandle};

#[derive(Clone)]
pub(in crate::xiaomi::adapter) struct Candidates {
    pub(in crate::xiaomi::adapter) hub: Result<SessionHandle, String>,
    pub(in crate::xiaomi::adapter) direct: Result<Client, String>,
    pub(in crate::xiaomi::adapter) cloud: Result<CloudPath, String>,
}

impl Candidates {
    pub(super) fn unavailable(reason: &str) -> Self {
        Self {
            hub: Err(reason.into()),
            direct: Err(reason.into()),
            cloud: Err(reason.into()),
        }
    }
    pub(in crate::xiaomi::adapter) fn kinds(&self) -> Vec<PathKind> {
        [
            (PathKind::Local, self.hub.is_ok()),
            (PathKind::Direct, self.direct.is_ok()),
            (PathKind::Cloud, self.cloud.is_ok()),
        ]
        .into_iter()
        .filter_map(|(kind, usable)| usable.then_some(kind))
        .collect()
    }

    pub(super) fn unavailable_reason(&self) -> String {
        format!(
            "hub: {}; direct: {}; cloud: {}",
            self.hub
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or("automatic reads paused while hub directory is degraded"),
            self.direct
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or("available"),
            self.cloud
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or("available"),
        )
    }
}

#[derive(Clone)]
pub(in crate::xiaomi::adapter) struct RouteChoice {
    pub(in crate::xiaomi::adapter) candidates: Candidates,
    pub(in crate::xiaomi::adapter) selected: Option<ControlPath>,
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Control,
    Read,
    Disabled,
}

/// Candidate eligibility is independent of whether an operation may select it.
pub(super) fn select<T>(
    priority: PathPriority,
    operation: Operation,
    hub: Option<T>,
    direct: Option<T>,
    cloud: Option<T>,
) -> Option<T> {
    match (priority, operation) {
        (_, Operation::Disabled) => None,
        (PathPriority::LocalFirst, _) => hub.or(direct).or(cloud),
        (PathPriority::CloudFirst, Operation::Control) => direct.or(cloud).or(hub),
        (PathPriority::CloudFirst, Operation::Read) => direct.or(cloud),
    }
}

impl RouteChoice {
    pub(super) fn new(
        candidates: Candidates,
        priority: PathPriority,
        operation: Operation,
    ) -> Self {
        let selected = select(
            priority,
            operation,
            candidates.hub.clone().ok().map(ControlPath::Local),
            candidates.direct.clone().ok().map(ControlPath::Direct),
            candidates.cloud.clone().ok().map(ControlPath::Cloud),
        );
        Self {
            candidates,
            selected,
        }
    }
}
