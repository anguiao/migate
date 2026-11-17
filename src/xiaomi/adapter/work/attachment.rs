use super::*;
use std::cell::{Ref, RefMut};

/// Native connection receipts and active subscriptions share one authority.
#[derive(Default)]
pub(super) struct NativeAttachments {
    pub(super) local: RefCell<LocalAttachment>,
    pub(super) cloud: RefCell<CloudAttachment>,
    pub(super) active: RefCell<BTreeMap<String, NotificationSource>>,
    condition_generation: Cell<u64>,
    retry_requested: Cell<bool>,
    pub(super) changed: Event,
}

impl NativeAttachments {
    pub(super) fn prepare(&self, source: &NotificationSource) -> Option<Ref<'_, Prepare>> {
        match source {
            NotificationSource::Local(handle) => Ref::filter_map(self.local.borrow(), |local| {
                local
                    .connection
                    .as_ref()
                    .filter(|current| current.same_connection(handle))
                    .map(|_| &local.prepare)
            })
            .ok(),
            NotificationSource::Cloud(handle) => Ref::filter_map(self.cloud.borrow(), |cloud| {
                cloud
                    .connection
                    .as_ref()
                    .filter(|current| current.same_connection(handle))
                    .map(|_| &cloud.prepare)
            })
            .ok(),
        }
    }
    pub(super) fn prepare_mut(&self, source: &NotificationSource) -> Option<RefMut<'_, Prepare>> {
        match source {
            NotificationSource::Local(handle) => {
                RefMut::filter_map(self.local.borrow_mut(), |local| {
                    local
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                        .then_some(&mut local.prepare)
                })
                .ok()
            }
            NotificationSource::Cloud(handle) => {
                RefMut::filter_map(self.cloud.borrow_mut(), |cloud| {
                    cloud
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                        .then_some(&mut cloud.prepare)
                })
                .ok()
            }
        }
    }
    pub(super) fn retire(&self, did: &str, property_subscriptions: bool) {
        let local = self.local.borrow_mut().connection.take();
        if let Some(local) = local {
            if property_subscriptions {
                drop(local.unsubscribe_properties(did));
                drop(local.unsubscribe_events(did));
            } else {
                drop(local.unsubscribe_device(did));
            }
        }
        self.local.borrow_mut().invalidate_prepare();
        self.cloud.borrow_mut().online.take();
        let cloud = self.cloud.borrow_mut().connection.take();
        if let Some(cloud) = cloud {
            drop(cloud.unsubscribe_device(did));
        }
        self.cloud.borrow_mut().prepare.invalidate();
        self.active.borrow_mut().clear();
        self.changed.notify(usize::MAX);
    }
}

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
    pub(super) fn finish(&mut self) {
        self.running = false;
        self.exhausted = !self.problems.is_empty();
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
    pub(in crate::xiaomi::adapter) fn observer(&self) -> Option<NotificationSource> {
        self.native
            .device()
            .functions()
            .iter()
            .find_map(|function| self.function_observer(function.id()))
    }
    pub(in crate::xiaomi::adapter) fn function_observer(
        &self,
        id: &str,
    ) -> Option<NotificationSource> {
        self.attachments.active.borrow().get(id).cloned()
    }
    pub(in crate::xiaomi::adapter) fn install_observer(
        &self,
        id: &str,
        source: NotificationSource,
    ) {
        self.install_observer_with_baseline(id, source, true);
    }
    pub(in crate::xiaomi::adapter) fn install_state_observer(
        &self,
        id: &str,
        source: NotificationSource,
    ) {
        self.install_observer_with_baseline(id, source, false);
    }
    fn install_observer_with_baseline(&self, id: &str, source: NotificationSource, baseline: bool) {
        for binding in self.channel_subscriptions(id) {
            match (&source, binding) {
                (NotificationSource::Local(handle), SubscriptionBinding::Property(binding)) => {
                    handle.invalidate_received_property(&self.key.id, binding)
                }
                (NotificationSource::Local(handle), SubscriptionBinding::Event(binding)) => {
                    handle.invalidate_received_event(&self.key.id, binding)
                }
                (NotificationSource::Cloud(handle), SubscriptionBinding::Property(binding)) => {
                    handle.invalidate_received_property(&self.key.id, binding)
                }
                (NotificationSource::Cloud(handle), SubscriptionBinding::Event(binding)) => {
                    handle.invalidate_received_event(&self.key.id, binding)
                }
            }
        }
        self.attachments
            .active
            .borrow_mut()
            .insert(id.to_owned(), source);
        if baseline {
            self.function_source_changed(id);
        } else {
            self.observations.function_source_changed(id);
        }
    }
    pub(in crate::xiaomi::adapter) fn revoke_observer(&self, id: &str) {
        if self.attachments.active.borrow_mut().remove(id).is_some() {
            self.function_source_changed(id);
        }
    }

    /// Attach one local connection. Repeating the same connection is
    /// idempotent; a different connection supersedes the old preparation
    /// receipt without touching the cloud attachment or observer.
    pub(in crate::xiaomi::adapter) fn attach_local(&self, handle: SessionHandle) -> bool {
        let mut local = self.attachments.local.borrow_mut();
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
        self.source_retired(PathKind::Local);
        true
    }
    /// Withdraw only this local attachment. A cloud observer remains valid.
    pub(in crate::xiaomi::adapter) fn detach_local(&self, handle: &SessionHandle) -> bool {
        let mut local = self.attachments.local.borrow_mut();
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
        self.source_retired(PathKind::Local);
        true
    }
    pub(in crate::xiaomi::adapter) fn local_attached(&self, handle: &SessionHandle) -> bool {
        self.attachments
            .local
            .borrow()
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
    }
    /// Attach one cloud notification connection. Repeating the same connection
    /// is idempotent; a new connection starts a fresh preparation generation.
    pub(in crate::xiaomi::adapter) fn attach_cloud(&self, handle: NotificationHandle) -> bool {
        let mut cloud = self.attachments.cloud.borrow_mut();
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
        self.source_retired(PathKind::Cloud);
        true
    }
    /// Withdraw only this cloud attachment. A local observer remains valid.
    pub(in crate::xiaomi::adapter) fn detach_cloud(&self, handle: &NotificationHandle) -> bool {
        let mut cloud = self.attachments.cloud.borrow_mut();
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
        self.source_retired(PathKind::Cloud);
        true
    }
    pub(in crate::xiaomi::adapter) fn cloud_attached(&self, handle: &NotificationHandle) -> bool {
        self.attachments
            .cloud
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
        let mut cloud = self.attachments.cloud.borrow_mut();
        if cloud.online.as_ref().is_some_and(|state| {
            !state.failed.get() || state.condition == self.attachments.condition_generation.get()
        }) {
            return None;
        }
        let state = Rc::new(OnlinePreparation {
            condition: self.attachments.condition_generation.get(),
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
                .attachments
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
        let handle = {
            let mut cloud = self.attachments.cloud.borrow_mut();
            cloud
                .online
                .take()
                .map(|_| cloud.connection.as_ref().unwrap().clone())
        };
        if let Some(handle) = handle {
            drop(handle.unsubscribe_online(&self.key.id));
        }
    }
    fn source_retired(&self, kind: PathKind) {
        let ids = self
            .attachments
            .active
            .borrow()
            .iter()
            .filter(|(_, source)| source.kind() == kind)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.revoke_observer(&id);
        }
        self.attachments.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn local_preparation_active(&self) -> bool {
        let local = self.attachments.local.borrow();
        local.connection.is_some()
            && (!local.prepare.subscribed.is_empty() || local.prepare.running)
    }
    pub(in crate::xiaomi::adapter) fn cloud_preparation_active(&self) -> bool {
        let cloud = self.attachments.cloud.borrow();
        cloud.connection.is_some()
            && (!cloud.prepare.subscribed.is_empty() || cloud.prepare.running)
    }
    fn retire_preparation(&self, source: &NotificationSource) {
        if matches!(source, NotificationSource::Local(_)) {
            self.attachments.local.borrow_mut().requested = false;
        }
        if let Some(mut prepare) = self.attachments.prepare_mut(source) {
            prepare.invalidate();
        }
    }
    fn retire_notifications(&self, source: NotificationSource) {
        self.retire_preparation(&source);
        match source {
            NotificationSource::Local(handle) => {
                drop(handle.unsubscribe_properties(&self.key.id));
                drop(handle.unsubscribe_events(&self.key.id));
            }
            NotificationSource::Cloud(handle) => {
                drop(handle.unsubscribe_properties(&self.key.id));
                drop(handle.unsubscribe_events(&self.key.id));
            }
        }
    }
    pub(in crate::xiaomi::adapter) fn retire_local_notifications(&self) {
        let handle = self.attachments.local.borrow().connection.clone();
        if let Some(handle) = handle {
            self.retire_notifications(NotificationSource::Local(handle));
        }
    }
    pub(in crate::xiaomi::adapter) fn retire_cloud_notifications(&self) {
        let handle = self.attachments.cloud.borrow().connection.clone();
        if let Some(handle) = handle {
            self.retire_notifications(NotificationSource::Cloud(handle));
        }
    }
    pub(in crate::xiaomi::adapter) fn local_exhausted(&self) -> bool {
        self.attachments.local.borrow().prepare.exhausted
    }
    pub(in crate::xiaomi::adapter) fn cloud_exhausted(&self) -> bool {
        self.attachments.cloud.borrow().prepare.exhausted
    }
    pub(in crate::xiaomi::adapter) fn notifications_preparing(&self) -> bool {
        self.attachments.local.borrow().prepare.running
            || self.attachments.cloud.borrow().prepare.running
    }
    pub(in crate::xiaomi::adapter) fn retry_subscriptions(&self) {
        self.attachments
            .condition_generation
            .set(self.attachments.condition_generation.get() + 1);
        self.attachments.local.borrow_mut().prepare.exhausted = false;
        self.attachments.cloud.borrow_mut().prepare.exhausted = false;
        self.attachments.retry_requested.set(true);
        self.attachments.changed.notify(usize::MAX);
    }
    pub(in crate::xiaomi::adapter) fn take_subscription_retry(&self) -> bool {
        self.attachments.retry_requested.replace(false)
    }
}
