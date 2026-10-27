use super::*;

/// All preparation state for one source connection. A failed preparation
/// remains exhausted until an actual condition change reopens it.
#[derive(Default)]
pub(super) struct Prepare {
    pub(super) subscribed: BTreeSet<SubscriptionBinding>,
    pub(super) problems: BTreeMap<SubscriptionBinding, String>,
    pub(super) running: bool,
    pub(super) exhausted: bool,
    pub(super) generation: u64,
}
impl Prepare {
    /// Withdraw this preparation receipt while retaining its diagnostic and
    /// exhausted condition until an actual access change reopens it.
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.subscribed.clear();
        self.running = false;
    }
    /// Start a fresh connection preparation, including a new retry window.
    pub(super) fn reset(&mut self) {
        self.invalidate();
        self.problems.clear();
        self.exhausted = false;
    }
    pub(super) fn begin(&mut self) -> Option<u64> {
        if self.running || self.exhausted {
            return None;
        }
        self.running = true;
        Some(self.generation)
    }
    pub(super) fn subscribed(&mut self, binding: SubscriptionBinding) {
        self.subscribed.insert(binding);
        self.problems.remove(&binding);
    }
    /// Keep optional battery failures separate from required source readiness.
    pub(super) fn finish(&mut self, battery: Option<PropertyBinding>) -> Option<String> {
        self.running = false;
        self.exhausted = !self.problems.is_empty();
        battery.and_then(|binding| self.problems.get(&binding.into()).cloned())
    }
    pub(super) fn problem(&self, bindings: &[SubscriptionBinding]) -> Option<String> {
        bindings
            .iter()
            .find_map(|binding| self.problems.get(binding).cloned())
    }
    pub(super) fn ready(&self, bindings: &[SubscriptionBinding]) -> bool {
        bindings
            .iter()
            .all(|binding| self.subscribed.contains(binding))
    }
    pub(super) fn missing(&self, bindings: &[SubscriptionBinding]) -> Vec<SubscriptionBinding> {
        bindings
            .iter()
            .copied()
            .filter(|binding| !self.subscribed.contains(binding))
            .collect()
    }
}
#[derive(Default)]
pub(super) struct LocalAttachment {
    pub(super) connection: Option<SessionHandle>,
    pub(super) prepare: Prepare,
    pub(super) requested: bool,
}
impl LocalAttachment {
    pub(super) fn invalidate_prepare(&mut self) {
        self.requested = false;
        self.prepare.invalidate();
    }
    pub(super) fn reset_prepare(&mut self) {
        self.requested = false;
        self.prepare.reset();
    }
}
#[derive(Default)]
pub(super) struct CloudAttachment {
    pub(super) connection: Option<NotificationHandle>,
    pub(super) prepare: Prepare,
    pub(super) online: Option<Rc<OnlinePreparation>>,
}
pub(in crate::xiaomi::adapter) struct OnlinePreparation {
    pub(super) condition: u64,
    pub(super) failed: Cell<bool>,
}
impl CloudAttachment {
    pub(super) fn reset_prepare(&mut self) {
        self.online = None;
        self.prepare.reset();
    }
}

impl DeviceWork {
    /// Attach one local connection. Repeating the same connection is
    /// idempotent; a different connection supersedes the old preparation
    /// receipt without touching the cloud attachment or observer.
    pub(in crate::xiaomi::adapter) fn attach_local(&self, handle: SessionHandle) -> bool {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(&handle))
        {
            return false;
        }
        let previous = local.connection.replace(handle);
        local.reset_prepare();
        drop(local);
        if let Some(previous) = previous {
            drop(previous.unsubscribe_properties(&self.key.id));
            drop(previous.unsubscribe_events(&self.key.id));
        }
        self.local_source_retired();
        true
    }
    /// Withdraw only this local attachment. A cloud observer remains valid.
    pub(in crate::xiaomi::adapter) fn detach_local(&self, handle: &SessionHandle) -> bool {
        let mut local = self.local.borrow_mut();
        if !local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
        {
            return false;
        }
        local.connection.take();
        local.reset_prepare();
        drop(local);
        self.local_source_retired();
        true
    }
    pub(in crate::xiaomi::adapter) fn local_attached(&self, handle: &SessionHandle) -> bool {
        self.local
            .borrow()
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
    }
    /// Attach one cloud notification connection. Repeating the same connection
    /// is idempotent; a new connection starts a fresh preparation generation.
    pub(in crate::xiaomi::adapter) fn attach_cloud(&self, handle: NotificationHandle) -> bool {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(&handle))
        {
            return false;
        }
        cloud.connection.replace(handle);
        cloud.reset_prepare();
        drop(cloud);
        self.cloud_source_retired();
        true
    }
    /// Withdraw only this cloud attachment. A local observer remains valid.
    pub(in crate::xiaomi::adapter) fn detach_cloud(&self, handle: &NotificationHandle) -> bool {
        let mut cloud = self.cloud.borrow_mut();
        if !cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
        {
            return false;
        }
        cloud.connection.take();
        cloud.reset_prepare();
        drop(cloud);
        self.cloud_source_retired();
        true
    }
    pub(in crate::xiaomi::adapter) fn cloud_attached(&self, handle: &NotificationHandle) -> bool {
        self.cloud
            .borrow()
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
    }
    pub(in crate::xiaomi::adapter) fn begin_online(
        &self,
        handle: &NotificationHandle,
    ) -> Option<Rc<OnlinePreparation>> {
        if self.life.is_closed() || !self.cloud_attached(handle) {
            return None;
        }
        let mut cloud = self.cloud.borrow_mut();
        if cloud.online.as_ref().is_some_and(|state| {
            !state.failed.get() || state.condition == self.condition_generation.get()
        }) {
            return None;
        }
        let state = Rc::new(OnlinePreparation {
            condition: self.condition_generation.get(),
            failed: Cell::new(false),
        });
        cloud.online = Some(state.clone());
        Some(state)
    }
    pub(in crate::xiaomi::adapter) fn online_current(
        &self,
        handle: &NotificationHandle,
        state: &Rc<OnlinePreparation>,
    ) -> bool {
        !self.life.is_closed()
            && self.cloud_attached(handle)
            && self
                .cloud
                .borrow()
                .online
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, state))
    }
    pub(in crate::xiaomi::adapter) fn online_failed(
        &self,
        handle: &NotificationHandle,
        state: &Rc<OnlinePreparation>,
    ) {
        if self.online_current(handle, state) {
            state.failed.set(true);
        }
    }
    pub(in crate::xiaomi::adapter) fn retire_online(&self) {
        let mut cloud = self.cloud.borrow_mut();
        if cloud.online.take().is_some() {
            drop(
                cloud
                    .connection
                    .as_ref()
                    .unwrap()
                    .unsubscribe_online(&self.key.id),
            );
        }
    }
    pub(super) fn local_source_retired(&self) {
        let ids = self
            .active
            .borrow()
            .iter()
            .filter(|(_, source)| matches!(source, NotificationSource::Local(_)))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.revoke_observer(&id);
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn cloud_source_retired(&self) {
        let ids = self
            .active
            .borrow()
            .iter()
            .filter(|(_, source)| matches!(source, NotificationSource::Cloud(_)))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.revoke_observer(&id);
        }
        self.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn local_preparation_active(&self) -> bool {
        let local = self.local.borrow();
        local.connection.is_some()
            && (!local.prepare.subscribed.is_empty() || local.prepare.running)
    }
    pub(in crate::xiaomi::adapter) fn cloud_preparation_active(&self) -> bool {
        let cloud = self.cloud.borrow();
        cloud.connection.is_some()
            && (!cloud.prepare.subscribed.is_empty() || cloud.prepare.running)
    }
    pub(super) fn retire_local_preparation(&self) {
        self.local.borrow_mut().invalidate_prepare();
    }
    pub(super) fn retire_cloud_preparation(&self) {
        self.cloud.borrow_mut().prepare.invalidate();
    }
    pub(in crate::xiaomi::adapter) fn retire_local_notifications(&self) {
        let handle = self.local.borrow().connection.clone();
        if let Some(handle) = handle {
            drop(handle.unsubscribe_properties(&self.key.id));
            drop(handle.unsubscribe_events(&self.key.id));
        }
        self.retire_local_preparation();
    }
    pub(in crate::xiaomi::adapter) fn retire_cloud_notifications(&self) {
        let handle = self.cloud.borrow().connection.clone();
        if let Some(handle) = handle {
            drop(handle.unsubscribe_properties(&self.key.id));
            drop(handle.unsubscribe_events(&self.key.id));
        }
        self.retire_cloud_preparation();
    }
    pub(in crate::xiaomi::adapter) fn local_exhausted(&self) -> bool {
        self.local.borrow().prepare.exhausted
    }
    pub(in crate::xiaomi::adapter) fn cloud_exhausted(&self) -> bool {
        self.cloud.borrow().prepare.exhausted
    }
    pub(in crate::xiaomi::adapter) fn notifications_preparing(&self) -> bool {
        self.local.borrow().prepare.running || self.cloud.borrow().prepare.running
    }
    pub(in crate::xiaomi::adapter) fn retry_subscriptions(&self) {
        self.condition_generation
            .set(self.condition_generation.get() + 1);
        self.local.borrow_mut().prepare.exhausted = false;
        self.cloud.borrow_mut().prepare.exhausted = false;
        self.retry_requested.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn take_subscription_retry(&self) -> bool {
        self.retry_requested.replace(false)
    }
}
