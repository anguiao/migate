//! Shared priority selection for control, reads and subscription preparation.

use super::{Client, CloudPath, ControlPath, PathKind, PathPriority, SessionHandle};

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

pub(super) fn select_kind(
    priority: PathPriority,
    operation: Operation,
    hub: bool,
    direct: bool,
    cloud: bool,
) -> Option<PathKind> {
    select(
        priority,
        operation,
        hub.then_some(PathKind::Local),
        direct.then_some(PathKind::Direct),
        cloud.then_some(PathKind::Cloud),
    )
}

impl RouteChoice {
    pub(in crate::xiaomi::adapter) fn selected_kind(&self) -> Option<PathKind> {
        self.selected.as_ref().map(ControlPath::kind)
    }

    pub(super) fn new(
        candidates: Candidates,
        priority: PathPriority,
        operation: Operation,
    ) -> Self {
        let kind = select_kind(
            priority,
            operation,
            candidates.hub.is_ok(),
            candidates.direct.is_ok(),
            candidates.cloud.is_ok(),
        );
        Self::from_selection(candidates, kind)
    }

    pub(super) fn from_selection(candidates: Candidates, kind: Option<PathKind>) -> Self {
        let selected = kind.map(|kind| match kind {
            PathKind::Local => ControlPath::Local(candidates.hub.as_ref().unwrap().clone()),
            PathKind::Direct => ControlPath::Direct(candidates.direct.as_ref().unwrap().clone()),
            PathKind::Cloud => ControlPath::Cloud(candidates.cloud.as_ref().unwrap().clone()),
        });
        Self {
            candidates,
            selected,
        }
    }
}
