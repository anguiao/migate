use super::{
    commands::LightWriteGate,
    model::AdapterError,
    observations::Observations,
    refresh::RefreshState,
    state::{Context, Lifetime, LogicalSink},
};
use crate::{
    configuration::NativeDescription,
    device::{ActionToken, DeviceKey, LightDimension, PhysicalDevice},
    xiaomi::{capability::PropertyBinding, central::SessionHandle, cloud::NotificationHandle},
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    time::Duration,
};

#[cfg(test)]
#[path = "tests/work_access.rs"]
mod test_access;

/// One captured source runtime and its logical sink. A repaired action keeps
/// the member universe captured at its first frame; the group itself uses the
/// same pair.
#[derive(Clone)]
pub(super) struct SourceRef {
    pub(super) work: Rc<DeviceWork>,
    pub(super) sink: Rc<LogicalSink>,
}

struct FrameOwnership {
    token: ActionToken,
    epoch: u64,
    members: Rc<Vec<SourceRef>>,
}

/// The newest RGB frame on a group function. A slow member checks ownership
/// before submitting so a newer frame, Stop or takeover retires queued writes
/// without cancelling a successful final acceptance.
#[derive(Default)]
pub(super) struct LightFrames {
    light: Option<FrameOwnership>,
}
impl LightFrames {
    pub(super) fn next_light(
        &mut self,
        token: ActionToken,
        snapshot: impl FnOnce() -> Vec<SourceRef>,
    ) -> (u64, Rc<Vec<SourceRef>>) {
        if let Some(frame) = self.light.as_mut()
            && frame.token == token
        {
            frame.epoch += 1;
            return (frame.epoch, frame.members.clone());
        }
        let members = Rc::new(snapshot());
        let epoch = 1;
        self.light = Some(FrameOwnership {
            token,
            epoch,
            members: members.clone(),
        });
        (epoch, members)
    }
    pub(super) fn current_light(&self, token: ActionToken, epoch: u64) -> bool {
        matches!(self.light, Some(ref frame) if frame.token == token && frame.epoch == epoch)
    }
    pub(super) fn cancel_light(&mut self, token: ActionToken, epoch: u64) {
        if self.current_light(token, epoch) {
            self.light = None;
        }
    }
    pub(super) fn cancel_dimension(&mut self, dimension: LightDimension) {
        if dimension == LightDimension::Color {
            self.light = None;
        }
    }

    /// A committed rebuild or runtime reset retires every queued frame.
    pub(super) fn cancel_all(&mut self) {
        self.light = None;
    }
}

/// All preparation state for one source connection. A failed preparation
/// remains exhausted until an actual condition change reopens it.
#[derive(Default)]
struct Prepare {
    subscribed: BTreeSet<(u32, u32)>,
    running: bool,
    exhausted: bool,
    problem: Option<String>,
    generation: u64,
}
impl Prepare {
    /// Withdraw this preparation receipt while retaining its diagnostic and
    /// exhausted condition until an actual access change reopens it.
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.subscribed.clear();
        self.running = false;
    }
    /// Start a fresh connection preparation, including a new retry window.
    fn reset(&mut self) {
        self.invalidate();
        self.exhausted = false;
        self.problem = None;
    }
    fn ready(&self, bindings: &[PropertyBinding]) -> bool {
        bindings
            .iter()
            .all(|binding| self.subscribed.contains(&(binding.siid, binding.piid)))
    }
    fn missing(&self, bindings: &[PropertyBinding]) -> Vec<PropertyBinding> {
        bindings
            .iter()
            .copied()
            .filter(|binding| !self.subscribed.contains(&(binding.siid, binding.piid)))
            .collect()
    }
}
#[derive(Default)]
struct LocalAttachment {
    connection: Option<SessionHandle>,
    prepare: Prepare,
    requested: bool,
}
impl LocalAttachment {
    fn invalidate_prepare(&mut self) {
        self.requested = false;
        self.prepare.invalidate();
    }
    fn reset_prepare(&mut self) {
        self.requested = false;
        self.prepare.reset();
    }
}
#[derive(Default)]
struct CloudAttachment {
    connection: Option<NotificationHandle>,
    prepare: Prepare,
}
impl CloudAttachment {
    fn invalidate_prepare(&mut self) {
        self.prepare.invalidate();
    }
    fn reset_prepare(&mut self) {
        self.prepare.reset();
    }
}

/// One installed native runtime: the immutable native control description and
/// the subscription/refresh/observation state attached to it. The logical
/// sink below is only the current delivery target and is swapped by a
/// logical-only installation without touching this native state.
pub(super) struct DeviceWork {
    pub(super) key: DeviceKey,
    pub(super) native: NativeDescription,
    /// Native authority; retiring or replacing it ends this runtime.
    pub(super) life: Lifetime,
    pub(super) observations: Observations,
    pub(super) light_write: Option<Rc<LightWriteGate>>,
    /// Current logical delivery target; logical-only replacement swaps it.
    pub(super) sink: RefCell<Rc<LogicalSink>>,
    local: RefCell<LocalAttachment>,
    cloud: RefCell<CloudAttachment>,
    active: RefCell<Option<super::notifications::NotificationSource>>,
    pub(super) condition_generation: Cell<u64>,
    pub(super) refresh: RefCell<RefreshState>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) light_frames: RefCell<LightFrames>,
    changed: Event,
}
impl DeviceWork {
    pub(super) fn new(
        native: NativeDescription,
        sink: Rc<LogicalSink>,
        refresh_changed: Rc<Event>,
        light_write: Option<Rc<LightWriteGate>>,
    ) -> Self {
        let key = native.device.key().clone();
        Self {
            key,
            native,
            life: Lifetime::default(),
            observations: Observations::default(),
            light_write,
            sink: RefCell::new(sink),
            local: RefCell::new(LocalAttachment::default()),
            cloud: RefCell::new(CloudAttachment::default()),
            active: RefCell::new(None),
            condition_generation: Cell::new(0),
            refresh: RefCell::new(RefreshState::default()),
            refresh_changed,
            light_frames: RefCell::new(LightFrames::default()),
            changed: Event::new(),
        }
    }
    pub(super) fn current_sink(&self) -> Rc<LogicalSink> {
        self.sink.borrow().clone()
    }
    /// Swap the logical delivery target and return the retired one.
    pub(super) fn replace_sink(&self, sink: Rc<LogicalSink>) -> Rc<LogicalSink> {
        std::mem::replace(&mut *self.sink.borrow_mut(), sink)
    }
    pub(super) fn physical(&self) -> PhysicalDevice {
        self.current_sink().physical.clone()
    }
    pub(super) fn binding(&self, id: &str) -> PropertyBinding {
        self.native
            .device
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap()
            .power()
    }
    fn is_light(&self) -> bool {
        self.native
            .device
            .functions()
            .iter()
            .any(|function| function.lighting().is_some())
    }

    /// Attach one local connection. Repeating the same connection is
    /// idempotent; a different connection supersedes the old preparation
    /// receipt without touching the cloud attachment or observer.
    pub(super) fn attach_local(&self, handle: SessionHandle) -> bool {
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
        }
        self.local_source_retired();
        true
    }
    /// Withdraw only this local attachment. A cloud observer remains valid.
    pub(super) fn detach_local(&self, handle: &SessionHandle) -> bool {
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
    pub(super) fn local_attached(&self, handle: &SessionHandle) -> bool {
        self.local
            .borrow()
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
    }
    /// Attach one cloud notification connection. Repeating the same connection
    /// is idempotent; a new connection starts a fresh preparation generation.
    pub(super) fn attach_cloud(&self, handle: NotificationHandle) -> bool {
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
    pub(super) fn detach_cloud(&self, handle: &NotificationHandle) -> bool {
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
    pub(super) fn cloud_attached(&self, handle: &NotificationHandle) -> bool {
        self.cloud
            .borrow()
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
    }
    fn local_source_retired(&self) {
        let active = matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Local(_))
        );
        if active {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.changed.notify(usize::MAX);
    }
    fn cloud_source_retired(&self) {
        let active = matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Cloud(_))
        );
        if active {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn observer(&self) -> Option<super::notifications::NotificationSource> {
        self.active.borrow().clone()
    }
    pub(super) fn observer_is(&self, source: &super::notifications::NotificationSource) -> bool {
        self.active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.same(source))
    }
    pub(super) fn replace_observer(
        &self,
        source: super::notifications::NotificationSource,
    ) -> Option<super::notifications::NotificationSource> {
        self.active.borrow_mut().replace(source)
    }
    pub(super) fn take_observer(&self) -> Option<super::notifications::NotificationSource> {
        self.active.borrow_mut().take()
    }
    pub(super) fn source_changed(&self) {
        self.observations.source_changed();
        self.physical().clear_light_state();
        super::refresh::cancel(self);
        self.refresh();
    }
    pub(super) fn notify_value(
        &self,
        binding: PropertyBinding,
        value: &serde_json::Value,
        cached_report: bool,
        takeover: bool,
    ) -> bool {
        if self.life.is_closed() {
            return false;
        }
        if self.native.device.refresh_trigger() == Some(binding) && value.as_bool().is_some() {
            self.refresh();
            return false;
        }
        let Some(function) = self
            .native
            .device
            .functions()
            .iter()
            .find(|function| super::lighting::bindings(function).contains(&binding))
        else {
            return false;
        };
        let id = function.id().to_owned();
        let light = function.lighting().is_some();
        if light {
            let origin = if cached_report {
                super::lighting::Origin::Cache
            } else {
                super::lighting::Origin::Notification
            };
            match super::lighting::observe(self, &id, binding, value, origin, takeover) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("Ignoring invalid light property: {error}");
                    false
                }
            }
        } else {
            if let Some(value) = value.as_bool() {
                self.observations.notify(self, &id, value);
            }
            false
        }
    }
    pub(super) fn request_local_subscriptions(&self) {
        let mut local = self.local.borrow_mut();
        if local.connection.is_none() || local.prepare.running || local.prepare.exhausted {
            return;
        }
        if !local.requested {
            local.requested = true;
            drop(local);
            self.changed.notify(usize::MAX);
        }
    }
    async fn local_preparation_request(&self) -> (SessionHandle, u64) {
        loop {
            let listener = self.changed.listen();
            let request = {
                let mut local = self.local.borrow_mut();
                let connection = local.connection.clone();
                if let Some(connection) = connection
                    && local.requested
                    && !local.prepare.running
                    && !local.prepare.exhausted
                {
                    local.requested = false;
                    local.prepare.running = true;
                    Some((connection, local.prepare.generation))
                } else {
                    None
                }
            };
            if let Some(request) = request {
                return request;
            }
            listener.await;
        }
    }
    fn local_current(&self, handle: &SessionHandle, generation: u64) -> bool {
        !self.life.is_closed()
            && self
                .local
                .borrow()
                .connection
                .as_ref()
                .is_some_and(|current| {
                    current.same_connection(handle)
                        && self.local.borrow().prepare.generation == generation
                })
    }
    fn local_pending(
        &self,
        handle: &SessionHandle,
        generation: u64,
    ) -> Option<Vec<PropertyBinding>> {
        let local = self.local.borrow();
        let current = local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
            && !self.life.is_closed();
        current.then(|| local.prepare.missing(&self.bindings()))
    }
    pub(super) fn local_subscribed(
        &self,
        handle: &SessionHandle,
        generation: u64,
        binding: PropertyBinding,
    ) {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
            && !self.life.is_closed()
        {
            local
                .prepare
                .subscribed
                .insert((binding.siid, binding.piid));
        }
    }
    pub(super) fn local_finished(
        &self,
        handle: &SessionHandle,
        generation: u64,
        problem: Option<String>,
    ) {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
        {
            local.prepare.running = false;
            local.prepare.exhausted = problem.is_some();
            local.prepare.problem = problem;
        }
    }
    fn local_abandoned(&self, handle: &SessionHandle, generation: u64) {
        let mut local = self.local.borrow_mut();
        if local
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && local.prepare.generation == generation
        {
            local.prepare.running = false;
        }
    }
    pub(super) fn begin_cloud_preparation(&self, handle: &NotificationHandle) -> Option<u64> {
        let mut cloud = self.cloud.borrow_mut();
        if !cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            || cloud.prepare.running
            || cloud.prepare.exhausted
            || self.life.is_closed()
        {
            return None;
        }
        cloud.prepare.running = true;
        Some(cloud.prepare.generation)
    }
    pub(super) fn cloud_current(&self, handle: &NotificationHandle, generation: u64) -> bool {
        !self.life.is_closed()
            && self
                .cloud
                .borrow()
                .connection
                .as_ref()
                .is_some_and(|current| {
                    current.same_connection(handle)
                        && self.cloud.borrow().prepare.generation == generation
                })
    }
    pub(super) fn cloud_pending(
        &self,
        handle: &NotificationHandle,
        generation: u64,
    ) -> Option<Vec<PropertyBinding>> {
        let cloud = self.cloud.borrow();
        let current = cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
            && !self.life.is_closed();
        current.then(|| cloud.prepare.missing(&self.bindings()))
    }
    pub(super) fn cloud_subscribed(
        &self,
        handle: &NotificationHandle,
        generation: u64,
        binding: PropertyBinding,
    ) {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
            && !self.life.is_closed()
        {
            cloud
                .prepare
                .subscribed
                .insert((binding.siid, binding.piid));
        }
    }
    pub(super) fn cloud_finished(
        &self,
        handle: &NotificationHandle,
        generation: u64,
        problem: Option<String>,
    ) {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
        {
            cloud.prepare.running = false;
            cloud.prepare.exhausted = problem.is_some();
            cloud.prepare.problem = problem;
        }
    }
    pub(super) fn cloud_abandoned(&self, handle: &NotificationHandle, generation: u64) {
        let mut cloud = self.cloud.borrow_mut();
        if cloud
            .connection
            .as_ref()
            .is_some_and(|current| current.same_connection(handle))
            && cloud.prepare.generation == generation
        {
            cloud.prepare.running = false;
        }
    }
    pub(super) fn source_ready(&self, source: &super::notifications::NotificationSource) -> bool {
        let bindings = self.bindings();
        match source {
            super::notifications::NotificationSource::Local(handle) => {
                let local = self.local.borrow();
                !self.life.is_closed()
                    && local
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                    && local.prepare.ready(&bindings)
            }
            super::notifications::NotificationSource::Cloud(handle) => {
                let cloud = self.cloud.borrow();
                !self.life.is_closed()
                    && cloud
                        .connection
                        .as_ref()
                        .is_some_and(|current| current.same_connection(handle))
                    && cloud.prepare.ready(&bindings)
            }
        }
    }
    pub(super) fn local_preparation_active(&self) -> bool {
        let local = self.local.borrow();
        local.connection.is_some()
            && (!local.prepare.subscribed.is_empty() || local.prepare.running)
    }
    pub(super) fn cloud_preparation_active(&self) -> bool {
        let cloud = self.cloud.borrow();
        cloud.connection.is_some()
            && (!cloud.prepare.subscribed.is_empty() || cloud.prepare.running)
    }
    pub(super) fn retire_local_preparation(&self) {
        self.local.borrow_mut().invalidate_prepare();
    }
    pub(super) fn retire_cloud_preparation(&self) {
        self.cloud.borrow_mut().invalidate_prepare();
    }
    pub(super) fn retire_local_notifications(&self) {
        let handle = self.local.borrow().connection.clone();
        if let Some(handle) = handle {
            drop(handle.unsubscribe_properties(&self.key.id));
        }
        self.retire_local_preparation();
    }
    pub(super) fn retire_cloud_notifications(&self) {
        let handle = self.cloud.borrow().connection.clone();
        if let Some(handle) = handle {
            drop(handle.unsubscribe_properties(&self.key.id));
        }
        self.retire_cloud_preparation();
    }
    pub(super) fn local_exhausted(&self) -> bool {
        self.local.borrow().prepare.exhausted
    }
    pub(super) fn cloud_exhausted(&self) -> bool {
        self.cloud.borrow().prepare.exhausted
    }
    pub(super) fn notifications_preparing(&self) -> bool {
        self.local.borrow().prepare.running || self.cloud.borrow().prepare.running
    }
    pub(super) fn retry_subscriptions(&self) {
        self.condition_generation
            .set(self.condition_generation.get() + 1);
        self.local.borrow_mut().prepare.exhausted = false;
        self.cloud.borrow_mut().prepare.exhausted = false;
        self.changed.notify(usize::MAX);
    }
    /// End this runtime's authority and notification preparation. Sent
    /// requests keep their own receipt task; only unsent reads and queued
    /// notification handshakes are retired.
    pub(super) fn retire(&self) {
        self.life.close();
        self.current_sink().close();
        let local = self.local.borrow_mut().connection.take();
        if let Some(local) = local {
            if self.is_light() {
                drop(local.unsubscribe_properties(&self.key.id));
            } else {
                drop(local.unsubscribe_device(&self.key.id));
            }
        }
        self.retire_local_preparation();
        let cloud = self.cloud.borrow_mut().connection.take();
        if let Some(cloud) = cloud {
            drop(cloud.unsubscribe_device(&self.key.id));
        }
        self.retire_cloud_preparation();
        self.take_observer();
        super::refresh::cancel(self);
        self.light_frames.borrow_mut().cancel_all();
        self.changed.notify(usize::MAX);
    }
    pub(super) fn refresh(&self) {
        for function in self.native.device.functions() {
            for binding in super::lighting::bindings(function) {
                self.refresh_property(function.id(), binding);
            }
        }
    }
    pub(super) fn refresh_property(&self, id: &str, binding: PropertyBinding) {
        self.observations.refresh(id, binding);
        self.refresh.borrow_mut().request(id, binding);
        self.refresh_changed.notify(usize::MAX);
    }
    /// Changes during an in-flight read coalesce into exactly one follow-up.
    pub(super) fn request_dirty_read(&self, id: &str, binding: PropertyBinding) {
        if !self.life.is_closed() && self.refresh.borrow_mut().request_dirty(id, binding) {
            self.observations.refresh(id, binding);
            self.refresh_changed.notify(usize::MAX);
        }
    }
    pub(super) fn ensure_property_read(&self, id: &str, binding: PropertyBinding) {
        if !self.refresh.borrow().contains(id, binding) {
            self.refresh_property(id, binding);
        }
    }
    pub(super) fn contains(&self, id: &str, binding: PropertyBinding) -> bool {
        self.native.device.functions().iter().any(|function| {
            function.id() == id && super::lighting::bindings(function).contains(&binding)
        })
    }
    pub(super) fn observe_read(
        &self,
        id: &str,
        binding: PropertyBinding,
        stamp: &super::observations::ReadStamp,
        value: &serde_json::Value,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        if !self.observations.read_is_current(self, id, binding, stamp) {
            return Ok(());
        }
        let light = self
            .native
            .device
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap()
            .lighting()
            .is_some();
        if light {
            super::lighting::observe(
                self,
                id,
                binding,
                value,
                super::lighting::Origin::Read,
                false,
            )?;
        } else {
            let value =
                value
                    .as_bool()
                    .ok_or(crate::xiaomi::capability::CapabilityError::InvalidValue(
                        "boolean",
                    ))?;
            self.observations.observe_read(self, id, stamp, value);
        }
        Ok(())
    }
    pub(super) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut values = self
            .native
            .device
            .functions()
            .iter()
            .flat_map(super::lighting::bindings)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.native.device.refresh_trigger() {
            values.push(trigger);
        }
        values
    }
    pub(super) fn diagnostic(&self) -> Option<String> {
        let local = self.local.borrow();
        let values = local
            .prepare
            .problem
            .clone()
            .into_iter()
            .chain(self.cloud.borrow().prepare.problem.clone())
            .chain(self.refresh.borrow().problems())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(super) fn subscription_problem(&self) -> Option<String> {
        self.local
            .borrow()
            .prepare
            .problem
            .clone()
            .or_else(|| self.cloud.borrow().prepare.problem.clone())
    }
    pub(super) fn report(&self, context: &Context, did: &str) {
        if self.life.is_closed() {
            return;
        }
        let blocked = context.live.borrow().devices[did].blocked.clone();
        let problem = blocked.or_else(|| self.diagnostic());
        context.problem(did, problem);
    }
    pub(super) fn problem(
        &self,
        operation: &str,
        binding: PropertyBinding,
        error: &impl std::fmt::Display,
    ) -> String {
        let message = format!(
            "{} {operation} {}.{}: {error}",
            self.key.id, binding.siid, binding.piid
        );
        log::warn!("{message}");
        message
    }
}
pub(super) async fn run(
    work: Rc<DeviceWork>,
    handle: SessionHandle,
    context: Rc<Context>,
) -> Result<(), AdapterError> {
    // Poll cancellation first so removed work cannot submit another request.
    future::or(
        async {
            work.life.ended().await;
            Ok(())
        },
        subscriptions(&work, &handle, &context),
    )
    .await
}
async fn subscriptions(
    work: &DeviceWork,
    handle: &SessionHandle,
    context: &Context,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    loop {
        let (connection, preparation) = work.local_preparation_request().await;
        if !connection.same_connection(handle) {
            work.local_abandoned(handle, preparation);
            continue;
        }
        if !context.current_local(handle) || !work.local_current(handle, preparation) {
            work.local_abandoned(handle, preparation);
            continue;
        }
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_millis(10))
            .await;
        let current = || context.current_local(handle) && work.local_current(handle, preparation);
        let Some(mut pending) = work.local_pending(handle, preparation) else {
            continue;
        };
        'attempts: for attempt in 0..4 {
            let mut failed = Vec::new();
            let mut problem = None;
            for binding in pending {
                if !current() {
                    break 'attempts;
                }
                let result = handle.subscribe_property(&did, binding).await;
                if !current() {
                    break 'attempts;
                }
                match result {
                    Ok(()) => work.local_subscribed(handle, preparation, binding),
                    Err(error) => {
                        problem = Some(work.problem("subscribe", binding, &error));
                        failed.push(binding);
                    }
                }
            }
            if failed.is_empty() || attempt == 3 {
                work.local_finished(handle, preparation, problem);
                work.report(context, &did);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_millis(100))
                .await;
        }
        work.local_abandoned(handle, preparation);
        context.changed.notify(usize::MAX);
    }
}
