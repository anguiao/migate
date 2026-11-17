use super::{
    commands::DeviceWriteGate,
    model::{
        AdapterError, NativeModeObservation, NativeValueObservation, PathKind, SubscriptionBinding,
        SubscriptionStatus,
    },
    notifications::NotificationSource,
    observations::{Observation, Observations, Origin},
    refresh::RefreshState,
    state::{Context, Lifetime},
};
use crate::{
    device::{ActionToken, DeviceKey, LightDimension, PhysicalDevice},
    xiaomi::capability::{
        NativeDescription, ParsedFunction, ReadInput, RelatedScope, SHARED_FEEDBACK_CHANNEL,
    },
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
use observation::{AirConditionerObservations, FanObservations, SharedFeedback};
mod sink;
mod subscriptions;

#[cfg(test)]
mod tests;

use attachment::NativeAttachments;
pub(super) use attachment::OnlinePreparation;
pub(super) use frames::LightFrames;
pub(super) use sink::LogicalSink;
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
    pub(super) id: String,
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
    /// Authentication work and the confirmed client share this native authority.
    pub(super) lan: RefCell<super::lan::DirectLan>,
    pub(super) observations: Observations,
    pub(super) light_write: Option<Rc<DeviceWriteGate>>,
    /// Current logical delivery target; logical-only replacement swaps it.
    sink: RefCell<Rc<LogicalSink>>,
    attachments: NativeAttachments,
    air_conditioner: AirConditionerObservations,
    fan: FanObservations,
    shared_feedback: SharedFeedback,
    pub(super) refresh: RefCell<RefreshState>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) light_frames: RefCell<LightFrames>,
    committed_battery_issue: RefCell<Option<String>>,
    battery_read_problem: RefCell<Option<String>>,
    configuration_changed: Rc<Event>,
}
impl DeviceWork {
    pub(super) fn new(
        native: NativeDescription,
        sink: Rc<LogicalSink>,
        refresh_changed: Rc<Event>,
        initial_reads_changed: Rc<Event>,
        configuration_changed: Rc<Event>,
        light_write: Option<Rc<DeviceWriteGate>>,
    ) -> Self {
        sink.initialize(&native);
        let key = native.device().key().clone();
        let battery_issue = native.device().battery_issue().map(str::to_owned);
        Self {
            key,
            native,
            life: Lifetime::default(),
            lan: RefCell::new(super::lan::DirectLan::default()),
            observations: Observations::default(),
            light_write,
            sink: RefCell::new(sink),
            attachments: NativeAttachments::default(),
            air_conditioner: AirConditionerObservations::default(),
            fan: FanObservations::default(),
            shared_feedback: SharedFeedback::default(),
            refresh: RefCell::new(RefreshState::new(initial_reads_changed)),
            refresh_changed,
            light_frames: RefCell::new(LightFrames::default()),
            committed_battery_issue: RefCell::new(battery_issue),
            battery_read_problem: RefCell::new(None),
            configuration_changed,
        }
    }
    fn reset_shared_fan_power(&self, id: &str) {
        self.current_sink().require_fan_power_read(id);
    }
    pub(super) fn invalidate_interlocked_blow(&self) {
        let Some(function) = self.native.device().functions().iter().find(|function| {
            function
                .fan()
                .and_then(|fan| fan.shared_speed())
                .is_some_and(|speed| {
                    speed.channel == crate::xiaomi::capability::SharedFanChannel::Blow
                })
        }) else {
            return;
        };
        let id = function.id();
        self.current_sink()
            .shared_fan_power(id)
            .borrow_mut()
            .require_read();
        self.observations
            .refresh(id, function.fan().unwrap().power());
    }
    pub(super) fn is_standard_mode_binding(
        &self,
        function: &str,
        binding: PropertyBinding,
    ) -> bool {
        self.native
            .plan()
            .standard_mode()
            .is_some_and(|mode| mode.function == function && mode.binding == binding)
    }
    pub(super) fn shared_feedback_plan(&self) -> bool {
        self.native.plan().shared_feedback().is_some()
    }
    pub(super) fn contains_input(&self, id: &str, input: ReadInput) -> bool {
        match input {
            ReadInput::Property(binding) => self.contains(id, binding),
            ReadInput::SharedFeedback => {
                id == SHARED_FEEDBACK_CHANNEL && self.shared_feedback_plan()
            }
        }
    }
    pub(super) fn refresh_input(&self, id: &str, input: ReadInput) {
        self.observations.refresh_input(id, input);
        self.refresh.borrow_mut().request_input(id, input, false);
        self.refresh_changed.notify(usize::MAX);
    }
    pub(super) fn problem_input(
        &self,
        kind: &str,
        input: ReadInput,
        error: &impl std::fmt::Display,
    ) -> String {
        format!("{kind} {input:?} failed: {error}")
    }
    pub(super) fn current_sink(&self) -> Rc<LogicalSink> {
        self.sink.borrow().clone()
    }
    /// Swap the logical delivery target and return the retired one.
    pub(super) fn replace_sink(&self, sink: Rc<LogicalSink>) -> Rc<LogicalSink> {
        sink.inherit_relays(&self.current_sink(), &self.native);
        sink.initialize(&self.native);
        let old = std::mem::replace(&mut *self.sink.borrow_mut(), sink);
        self.refresh.borrow_mut().reset_initial();
        let shared_available = self.refresh.borrow().shared_read_available();
        if let Some(available) = shared_available {
            self.set_shared_temperature_source_available(available);
        }
        for id in self.channels() {
            self.function_source_changed(&id);
        }
        self.refresh_shared_feedback();
        old
    }
    pub(super) fn physical(&self) -> PhysicalDevice {
        self.current_sink().physical.clone()
    }
    pub(super) fn resolve_initial(&self, id: &str, input: impl Into<ReadInput>) {
        if !self.life.is_closed() {
            self.refresh.borrow_mut().resolve_initial(id, input.into());
        }
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
            .flat_map(ParsedFunction::notification_bindings)
            .collect::<Vec<_>>();
        values.extend(
            self.native
                .device()
                .functions()
                .iter()
                .filter_map(|function| Some(function.heater()?.power_refresh_trigger()?.property)),
        );
        if let Some(trigger) = self.native.device().refresh_trigger() {
            values.push(trigger);
        }
        if let Some(mode) = self.native.plan().standard_mode() {
            values.push(mode.binding);
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
    fn function_source_changed(&self, id: &str) {
        self.reset_shared_fan_power(id);
        self.observations.function_source_changed(id);
        if let Some(function) = self.native.device().functions().iter().find(|function| {
            function.id() == id
                && (function.curtain().is_some()
                    || function.fan().is_some()
                    || function.air_conditioner().is_some()
                    || function.heater().is_some())
        }) {
            if function.curtain().is_some() {
                self.physical()
                    .function(id)
                    .unwrap()
                    .curtain_source_changed();
            }
            if function.air_conditioner().is_some() {
                self.physical()
                    .function(id)
                    .unwrap()
                    .air_conditioner_source_changed();
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
        self.lan.take().stop();
        self.refresh_changed.notify(usize::MAX);
        self.refresh.borrow().notify_initial_changed();
        self.current_sink().close();
        let property_subscriptions = self.is_light()
            || self.native.device().functions().iter().any(|function| {
                function.curtain().is_some()
                    || function.fan().is_some()
                    || function.air_conditioner().is_some()
                    || function.heater().is_some()
            });
        self.attachments
            .retire(&self.key.id, property_subscriptions);
        super::refresh::cancel(self);
        self.light_frames.borrow_mut().cancel_all();
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
                    && function.air_conditioner().is_none()
                    && function.heater().is_none()
        }) {
            for binding in function.observation_bindings() {
                self.refresh_property(function.id(), binding);
            }
            if let Some(heater) = function.heater() {
                for binding in heater.diagnostic_bindings() {
                    self.refresh_property(function.id(), binding);
                }
            }
        }
        if let Some(binding) = self.battery_binding() {
            self.refresh_property(BATTERY_CHANNEL, binding);
        }
        self.refresh_shared_feedback();
    }
    /// One related-read demand selected by the v13 source plan. Only a real
    /// new notification or a relevant write with an accepted/uncertain step
    /// calls this; ordinary reads never trigger it, so periodic reads cannot loop.
    pub(super) fn refresh_related_inputs(&self, scope: RelatedScope) {
        if self.life.is_closed() {
            return;
        }
        let Some(related) = self.native.plan().related() else {
            return;
        };
        for input in related.inputs(scope) {
            self.refresh_property(&input.function, input.binding);
        }
        if related.includes_shared() {
            self.observations
                .invalidate_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback);
            self.shared_feedback.await_feedback();
            self.refresh_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback);
        }
    }
    pub(super) fn related_scope_for_command(
        &self,
        channel: &str,
        target: &super::model::CommandTarget,
    ) -> Option<RelatedScope> {
        use crate::device::HeaterCommand;
        let function = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == channel)?;
        if self.native.plan().related().is_none()
            || function.lighting().is_some()
            || function.state().is_some()
            || function.measurement().is_some()
            || function.curtain().is_some()
            || function.air_conditioner().is_some()
        {
            return None;
        }
        match target {
            super::model::CommandTarget::Heater(HeaterCommand::Temperature(_)) => {
                Some(RelatedScope::Temperature)
            }
            super::model::CommandTarget::Heater(_) | super::model::CommandTarget::Fan(_) => {
                Some(RelatedScope::All)
            }
            _ => None,
        }
    }
    /// Invalidate an earlier shared read at the actual send boundary. This
    /// changes only the observation generation; it does not queue a read or
    /// pretend a new state was obtained.
    pub(super) fn invalidate_shared_read_for_function(&self, id: &str) {
        if !self.shared_feedback_plan() {
            return;
        }
        let relevant = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .is_some_and(|function| function.heater().is_some() || function.fan().is_some());
        if relevant {
            self.observations
                .invalidate_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback);
        }
    }
    pub(super) fn related_scope_for_notification(
        &self,
        channel: &str,
        binding: PropertyBinding,
    ) -> Option<RelatedScope> {
        self.native
            .plan()
            .related()
            .filter(|related| {
                related
                    .inputs(RelatedScope::All)
                    .iter()
                    .any(|input| input.function == channel && input.binding == binding)
            })
            .map(|_| RelatedScope::All)
    }
    pub(super) fn refresh_shared_feedback(&self) {
        if self.life.is_closed() || !self.shared_feedback_plan() {
            return;
        }
        self.refresh_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback);
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
        self.is_standard_mode_binding(id, binding)
            || self.native.device().functions().iter().any(|function| {
                function.id() == id
                    && (function.observation_bindings().contains(&binding)
                        || function
                            .heater()
                            .is_some_and(|heater| heater.diagnostic_bindings().contains(&binding)))
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
