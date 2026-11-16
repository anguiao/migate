//! Subscription eligibility, preparation preference and ready active sources.

use super::choices::{Operation, select};
use super::{DeviceWork, NotificationHandle, NotificationSource, PathPriority, Rc, SessionHandle};

#[derive(Clone)]
pub(in crate::xiaomi::adapter) struct NotificationRoutes {
    local: Option<NotificationSource>,
    cloud: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) desired: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) active: Option<NotificationSource>,
    pub(in crate::xiaomi::adapter) sync_limited_reason: Option<String>,
}

impl NotificationRoutes {
    pub(in crate::xiaomi::adapter) fn valid(&self, source: &NotificationSource) -> bool {
        self.local
            .iter()
            .chain(self.cloud.iter())
            .any(|candidate| candidate.same(source))
    }
}

pub(super) struct NotificationFacts<'a> {
    work: Option<&'a DeviceWork>,
    priority: PathPriority,
    local: Option<NotificationSource>,
    cloud: Option<NotificationSource>,
    local_exhausted: bool,
    cloud_exhausted: bool,
    virtual_group: bool,
    preparing: bool,
    problem: Option<String>,
    hub_problem: Option<String>,
}

impl<'a> NotificationFacts<'a> {
    pub(super) fn new(
        work: Option<&'a Rc<DeviceWork>>,
        hub: &Result<SessionHandle, String>,
        cloud: Option<&NotificationHandle>,
        virtual_group: bool,
        priority: PathPriority,
    ) -> Self {
        let local = hub
            .as_ref()
            .ok()
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
            local_exhausted: work.is_some_and(|work| work.local_exhausted()),
            cloud_exhausted: work.is_some_and(|work| work.cloud_exhausted()),
            preparing: work.is_some_and(|work| work.notifications_preparing()),
            problem: work.and_then(|work| work.subscription_problem()),
            hub_problem: hub.as_ref().err().cloned(),
        }
    }

    pub(super) fn reason(&self, cloud: &Result<(), String>) -> String {
        if self.virtual_group {
            "member feedback unavailable or subscriptions not ready".into()
        } else if self.preparing {
            "notification subscriptions preparing".into()
        } else if let Some(problem) = &self.problem {
            problem.clone()
        } else {
            let hub = self
                .hub_problem
                .as_deref()
                .unwrap_or("notification subscriptions not ready");
            let cloud = cloud
                .as_ref()
                .err()
                .map(String::as_str)
                .unwrap_or("notifications disconnected or subscriptions not ready");
            format!("hub notifications: {hub}; cloud notifications: {cloud}")
        }
    }

    pub(super) fn function(
        &self,
        id: &str,
        eligibility: &Result<(), String>,
    ) -> NotificationRoutes {
        let work = self.work.expect("installed notification bindings");
        let local = self.local.clone();
        let cloud = self.cloud.clone().filter(|_| eligibility.is_ok());
        let local_ready = local
            .as_ref()
            .is_some_and(|source| work.source_ready(id, source));
        let cloud_ready = cloud
            .as_ref()
            .is_some_and(|source| work.source_ready(id, source));
        let desired_local = local
            .clone()
            .filter(|_| !self.local_exhausted || local_ready);
        let desired_cloud = cloud
            .clone()
            .filter(|_| !self.cloud_exhausted || cloud_ready);
        let desired = select(
            self.priority,
            Operation::Control,
            desired_local,
            None,
            desired_cloud,
        );
        let active = work.function_observer(id).filter(|source| {
            local
                .as_ref()
                .is_some_and(|candidate| local_ready && candidate.same(source))
                || cloud
                    .as_ref()
                    .is_some_and(|candidate| cloud_ready && candidate.same(source))
        });
        let sync_limited_reason = active.is_none().then(|| {
            work.function_subscription_problem(id)
                .unwrap_or_else(|| self.reason(eligibility))
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
