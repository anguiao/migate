use super::{
    commands::DeviceWriteGate,
    model::{
        AdapterError, NativeModeObservation, PathKind, SubscriptionBinding, SubscriptionStatus,
    },
    notifications::NotificationSource,
    observations::{Observation, Observations, Origin},
    refresh::RefreshState,
    state::{Context, Lifetime, LogicalSink},
};
use crate::{
    device::{ActionToken, DeviceKey, LightDimension, PhysicalDevice},
    xiaomi::capability::{NativeDescription, ParsedFunction},
    xiaomi::{
        capability::{EventBinding, PropertyBinding},
        central::SessionHandle,
        cloud::NotificationHandle,
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Duration,
};

mod attachment;
mod frames;
mod observation;
mod subscriptions;

pub(super) use attachment::OnlinePreparation;
use attachment::{CloudAttachment, LocalAttachment};
pub(super) use frames::LightFrames;
use subscriptions::subscriptions;

const BATTERY_CHANNEL: &str = "battery";

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
    pub(super) light_write: Option<Rc<DeviceWriteGate>>,
    /// Current logical delivery target; logical-only replacement swaps it.
    sink: RefCell<Rc<LogicalSink>>,
    local: RefCell<LocalAttachment>,
    cloud: RefCell<CloudAttachment>,
    active: RefCell<BTreeMap<String, NotificationSource>>,
    observation_sources: RefCell<BTreeMap<String, PathKind>>,
    property_sources: RefCell<BTreeMap<super::observations::PropertyKey, PathKind>>,
    fan_modes: RefCell<BTreeMap<String, NativeModeObservation>>,
    state_read_paths: RefCell<BTreeMap<String, Option<super::routing::ControlPath>>>,
    condition_generation: Cell<u64>,
    pub(super) refresh: RefCell<RefreshState>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) light_frames: RefCell<LightFrames>,
    committed_battery_issue: RefCell<Option<String>>,
    battery_subscription_problem: RefCell<Option<String>>,
    battery_read_problem: RefCell<Option<String>>,
    retry_requested: Cell<bool>,
    changed: Event,
}
impl DeviceWork {
    pub(super) fn new(
        native: NativeDescription,
        sink: Rc<LogicalSink>,
        refresh_changed: Rc<Event>,
        light_write: Option<Rc<DeviceWriteGate>>,
    ) -> Self {
        let key = native.device().key().clone();
        let battery_issue = native.device().battery_issue().map(str::to_owned);
        Self {
            key,
            native,
            life: Lifetime::default(),
            observations: Observations::default(),
            light_write,
            sink: RefCell::new(sink),
            local: RefCell::new(LocalAttachment::default()),
            cloud: RefCell::new(CloudAttachment::default()),
            active: RefCell::new(BTreeMap::new()),
            observation_sources: RefCell::new(BTreeMap::new()),
            property_sources: RefCell::new(BTreeMap::new()),
            fan_modes: RefCell::new(BTreeMap::new()),
            state_read_paths: RefCell::new(BTreeMap::new()),
            condition_generation: Cell::new(0),
            refresh: RefCell::new(RefreshState::default()),
            refresh_changed,
            light_frames: RefCell::new(LightFrames::default()),
            committed_battery_issue: RefCell::new(battery_issue),
            battery_subscription_problem: RefCell::new(None),
            battery_read_problem: RefCell::new(None),
            retry_requested: Cell::new(false),
            changed: Event::new(),
        }
    }
    pub(super) fn fan_mode_observation(&self, id: &str) -> NativeModeObservation {
        self.fan_modes.borrow().get(id).copied().unwrap_or_default()
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
        if id == BATTERY_CHANNEL {
            return self
                .battery_binding()
                .expect("battery channel has a native binding");
        }
        self.native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap()
            .power()
            .expect("light and outlet functions have power")
    }
    fn battery_binding(&self) -> Option<PropertyBinding> {
        self.native
            .device()
            .battery()
            .map(crate::xiaomi::capability::BatteryBinding::property)
    }
    fn required_bindings(&self) -> Vec<PropertyBinding> {
        let mut values = self
            .native
            .device()
            .functions()
            .iter()
            .flat_map(ParsedFunction::observation_bindings)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.native.device().refresh_trigger() {
            values.push(trigger);
        }
        values
    }
    fn is_light(&self) -> bool {
        self.native
            .device()
            .functions()
            .iter()
            .any(|function| function.lighting().is_some())
    }

    pub(super) fn channels(&self) -> Vec<String> {
        self.native
            .device()
            .functions()
            .iter()
            .map(|function| function.id().to_owned())
            .chain(self.battery_binding().map(|_| BATTERY_CHANNEL.to_owned()))
            .collect()
    }
    pub(super) fn observer(&self) -> Option<NotificationSource> {
        self.native
            .device()
            .functions()
            .iter()
            .find_map(|function| self.function_observer(function.id()))
    }
    pub(super) fn function_observer(&self, id: &str) -> Option<NotificationSource> {
        self.active.borrow().get(id).cloned()
    }
    pub(super) fn install_observer(&self, id: &str, source: NotificationSource) {
        self.install_observer_with_baseline(id, source, true);
    }
    pub(super) fn install_state_observer(&self, id: &str, source: NotificationSource) {
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
        self.active.borrow_mut().insert(id.to_owned(), source);
        if baseline {
            self.function_source_changed(id);
        } else {
            self.observations.function_source_changed(id);
        }
    }
    pub(super) fn revoke_observer(&self, id: &str) {
        if self.active.borrow_mut().remove(id).is_some() {
            self.function_source_changed(id);
        }
    }
    fn function_source_changed(&self, id: &str) {
        self.observations.function_source_changed(id);
        if let Some(function) = self.native.device().functions().iter().find(|function| {
            function.id() == id && (function.curtain().is_some() || function.fan().is_some())
        }) {
            if function.curtain().is_some() {
                self.physical()
                    .function(id)
                    .unwrap()
                    .curtain_source_changed();
            }
            for binding in function.observation_bindings() {
                self.refresh.borrow_mut().request_baseline(id, binding);
            }
            self.refresh_changed.notify(usize::MAX);
            return;
        }
        if self.state_function(id) {
            let binding = self
                .native
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == id)
                .unwrap()
                .state()
                .unwrap()
                .property();
            self.refresh.borrow_mut().request_baseline(id, binding);
            self.refresh_changed.notify(usize::MAX);
            return;
        }
        if self
            .native
            .device()
            .functions()
            .iter()
            .any(|function| function.id() == id && function.lighting().is_some())
        {
            self.physical().clear_light_state();
        }
        super::refresh::cancel_function(self, id);
        let bindings = if id == BATTERY_CHANNEL {
            self.battery_binding().into_iter().collect()
        } else {
            self.native
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == id)
                .unwrap()
                .bindings()
        };
        for binding in bindings {
            self.refresh_property(id, binding);
        }
    }
    /// End this runtime's authority and notification preparation. Sent
    /// requests keep their own receipt task; only unsent reads and queued
    /// notification handshakes are retired.
    pub(super) fn retire(&self) {
        self.life.close();
        self.current_sink().close();
        let local = self.local.borrow_mut().connection.take();
        if let Some(local) = local {
            if self.is_light()
                || self
                    .native
                    .device()
                    .functions()
                    .iter()
                    .any(|function| function.curtain().is_some() || function.fan().is_some())
            {
                drop(local.unsubscribe_properties(&self.key.id));
                drop(local.unsubscribe_events(&self.key.id));
            } else {
                drop(local.unsubscribe_device(&self.key.id));
            }
        }
        self.retire_local_preparation();
        self.cloud.borrow_mut().online.take();
        let cloud = self.cloud.borrow_mut().connection.take();
        if let Some(cloud) = cloud {
            drop(cloud.unsubscribe_device(&self.key.id));
        }
        self.retire_cloud_preparation();
        self.active.borrow_mut().clear();
        super::refresh::cancel(self);
        self.light_frames.borrow_mut().cancel_all();
        self.changed.notify(usize::MAX);
    }
    pub(super) fn refresh(&self) {
        self.refresh_functions(true);
    }
    pub(super) fn refresh_connection(&self) {
        self.refresh_functions(false);
    }
    fn refresh_functions(&self, include_states: bool) {
        for function in self.native.device().functions().iter().filter(|function| {
            include_states
                || function.state().is_none()
                    && function.curtain().is_none()
                    && function.fan().is_none()
        }) {
            for binding in function.observation_bindings() {
                self.refresh_property(function.id(), binding);
            }
        }
        if let Some(binding) = self.battery_binding() {
            self.refresh_property(BATTERY_CHANNEL, binding);
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
        if id == BATTERY_CHANNEL {
            return self.battery_binding() == Some(binding);
        }
        self.native.device().functions().iter().any(|function| {
            function.id() == id && function.observation_bindings().contains(&binding)
        })
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
