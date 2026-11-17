//! Subscription eligibility, preparation preference and ready active sources.

use super::choices::{Operation, select};
use super::{DeviceWork, NotificationHandle, NotificationSource, PathPriority, Rc, SessionHandle};

pub(in crate::xiaomi::adapter) struct NotificationRoutes {
    local: Option<NotificationSource>,
    cloud: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) desired: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) active: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) sync_limited_reason: Option<String>,
}

impl NotificationRoutes {
    pub(in crate::xiaomi::adapter) fn sources(&self) -> impl Iterator<Item = &NotificationSource> {
        self.local.iter().chain(self.cloud.iter())
    }
    pub(in crate::xiaomi::adapter) fn valid(&self, source: &NotificationSource) -> bool {
        self.sources().any(|candidate| candidate.same(source))
    }
}

pub(super) struct NotificationBindings {
    local: Option<NotificationSource>,
    cloud: Option<NotificationSource>,
    local_ready: bool,
    cloud_ready: bool,
    pub(super) active: Option<NotificationSource>,
}

pub(super) struct NotificationFacts<'a> {
    work: Option<&'a DeviceWork>,
    priority: PathPriority,
    local: Option<NotificationSource>,
    cloud: Option<NotificationSource>,
    virtual_group: bool,
}

impl<'a> NotificationFacts<'a> {
    pub(super) fn new(
        work: Option<&'a Rc<DeviceWork>>,
        hub: Option<&SessionHandle>,
        cloud: Option<&NotificationHandle>,
        virtual_group: bool,
        priority: PathPriority,
    ) -> Self {
        let local = hub
            .filter(|handle| !virtual_group && work.is_some_and(|work| work.local_attached(handle)))
            .cloned()
            .map(NotificationSource::Local);
        let cloud = cloud
            .filter(|handle| !virtual_group && work.is_some_and(|work| work.cloud_attached(handle)))
            .cloned()
            .map(NotificationSource::Cloud);
        Self {
            work: work.map(Rc::as_ref),
            priority,
            local,
            cloud,
            virtual_group,
        }
    }

    pub(super) fn reason(&self, cloud: &Result<(), String>, hub_problem: Option<&str>) -> String {
        if self.virtual_group {
            "member feedback unavailable or subscriptions not ready".into()
        } else if self.work.is_some_and(|work| work.notifications_preparing()) {
            "notification subscriptions preparing".into()
        } else if let Some(problem) = self.work.and_then(|work| work.subscription_problem()) {
            problem
        } else {
            let hub = hub_problem.unwrap_or("notification subscriptions not ready");
            let cloud = cloud
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or("notifications disconnected or subscriptions not ready");
            format!("hub notifications: {hub}; cloud notifications: {cloud}")
        }
    }

    pub(super) fn bindings(&self, id: &str, cloud_eligible: bool) -> NotificationBindings {
        let work = self.work.expect("installed notification bindings");
        let local = self.local.clone();
        let cloud = self.cloud.clone().filter(|_| cloud_eligible);
        let local_ready = local
            .as_ref()
            .is_some_and(|source| work.source_ready(id, source));
        let cloud_ready = cloud
            .as_ref()
            .is_some_and(|source| work.source_ready(id, source));
        let active = work.function_observer(id).filter(|source| {
            local
                .as_ref()
                .is_some_and(|candidate| local_ready && candidate.same(source))
                || cloud
                    .as_ref()
                    .is_some_and(|candidate| cloud_ready && candidate.same(source))
        });
        NotificationBindings {
            local,
            cloud,
            local_ready,
            cloud_ready,
            active,
        }
    }

    pub(super) fn function(
        &self,
        id: &str,
        eligibility: &Result<(), String>,
        bindings: &NotificationBindings,
        hub_problem: Option<&str>,
    ) -> NotificationRoutes {
        let work = self.work.expect("installed notification bindings");
        let local = bindings.local.clone();
        let cloud = bindings.cloud.clone();
        let desired_local = local
            .clone()
            .filter(|_| !work.local_exhausted() || bindings.local_ready);
        let desired_cloud = cloud
            .clone()
            .filter(|_| !work.cloud_exhausted() || bindings.cloud_ready);
        let desired = select(
            self.priority,
            Operation::Control,
            desired_local,
            None,
            desired_cloud,
        );
        let active = bindings.active.clone();
        let sync_limited_reason = active.is_none().then(|| {
            work.function_subscription_problem(id)
                .unwrap_or_else(|| self.reason(eligibility, hub_problem))
        });
        NotificationRoutes {
            local,
            cloud,
            desired,
            active,
            sync_limited_reason,
        }
    }
}
