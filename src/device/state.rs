mod observation;
mod runtime;

use runtime::AmbientSource;
pub(super) use runtime::{AirConditionerRuntime, RuntimeState};

use super::{
    command::CommandReceiver,
    model::{
        AmbientTemperature, Availability, BatterySnapshot, FunctionDescription, FunctionKey,
        FunctionSnapshot, PhysicalDeviceDescription, Power,
    },
};
use event_listener::Event;
use std::sync::{Arc, Mutex};

struct PhysicalState {
    availability: Availability,
    revision: u64,
}
struct BatteryState {
    snapshot: BatterySnapshot,
    revision: u64,
}
struct PhysicalInner {
    state: Mutex<PhysicalState>,
    battery: Mutex<Option<BatteryState>>,
    battery_changed: Event,
    changed: Event,
}
pub(super) struct Inner {
    description: FunctionDescription,
    key: FunctionKey,
    physical: Arc<PhysicalInner>,
    pub(super) state: Mutex<RuntimeState>,
    changed: Event,
    pub(super) submitted: Event,
    pub(super) light_changed: Event,
}

/// Clear runtime light state in place; outlets keep their existing power cache.
fn clear_light_state(description: &FunctionDescription, state: &mut RuntimeState) -> bool {
    let Some((power, before_lighting)) = state.light_parts() else {
        return false;
    };
    let mut lighting = before_lighting;
    lighting.brightness_percent = None;
    lighting.temperature_kelvin = None;
    lighting.color = None;
    lighting.mode = description.inferred_color_mode();
    let before = (power, before_lighting);
    let after = (Power::Unknown, lighting);
    if before != after {
        state.set_light_parts(after.0, after.1);
        state.revision = state.revision.wrapping_add(1);
        true
    } else {
        false
    }
}

/// A function's shared state and optional command boundary for every adapter.
#[derive(Clone)]
pub struct Function(pub(super) Arc<Inner>);

/// Availability belongs to the physical device; each function has its own state and queue.
#[derive(Clone)]
pub struct PhysicalDevice {
    key: super::DeviceKey,
    label: Arc<Mutex<String>>,
    inner: Arc<PhysicalInner>,
    functions: Arc<[Function]>,
    has_battery: bool,
}
impl PhysicalDevice {
    pub fn new(description: PhysicalDeviceDescription) -> (Self, Vec<CommandReceiver>) {
        let (key, label, functions, battery) = description.into_parts();
        let inner = Arc::new(PhysicalInner {
            state: Mutex::new(PhysicalState {
                availability: Availability::Initializing,
                revision: 0,
            }),
            battery: Mutex::new(battery.then_some(BatteryState {
                snapshot: BatterySnapshot {
                    value: None,
                    source_time: None,
                    received_at: None,
                },
                revision: 0,
            })),
            battery_changed: Event::new(),
            changed: Event::new(),
        });
        let (functions, receivers): (Vec<_>, Vec<_>) = functions
            .into_iter()
            .map(|function| Function::new(key.clone(), function, inner.clone()))
            .unzip();
        let receivers = receivers.into_iter().flatten().collect();
        (
            Self {
                key,
                label: Arc::new(Mutex::new(label)),
                inner,
                functions: functions.into(),
                has_battery: battery,
            },
            receivers,
        )
    }
    pub fn key(&self) -> &super::DeviceKey {
        &self.key
    }
    pub fn label(&self) -> String {
        self.label.lock().unwrap().clone()
    }
    pub fn set_label(&self, label: String) {
        *self.label.lock().unwrap() = label;
    }
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }
    pub fn has_battery(&self) -> bool {
        self.has_battery
    }
    pub fn battery_snapshot(&self) -> Option<BatterySnapshot> {
        self.inner
            .battery
            .lock()
            .unwrap()
            .as_ref()
            .map(|battery| battery.snapshot)
    }
    /// Apply one physical battery percentage validated at the source boundary.
    pub fn observe_battery(
        &self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> Result<BatterySnapshot, super::CommandError> {
        let mut state = self.inner.battery.lock().unwrap();
        let Some(battery) = state.as_mut() else {
            return Err(super::CommandError::Unsupported);
        };
        let snapshot = BatterySnapshot {
            value: Some(value),
            source_time,
            received_at: Some(received_at),
        };
        if battery.snapshot != snapshot {
            battery.snapshot = snapshot;
            battery.revision = battery.revision.wrapping_add(1);
            self.inner.battery_changed.notify(usize::MAX);
        }
        Ok(snapshot)
    }
    pub(crate) fn subscribe_battery(&self) -> BatteryChanges {
        BatteryChanges {
            physical: self.inner.clone(),
            revision: self
                .inner
                .battery
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |battery| battery.revision),
        }
    }
    pub fn function(&self, id: &str) -> Option<Function> {
        self.functions
            .iter()
            .find(|function| function.function_key().id == id)
            .cloned()
    }
    pub fn availability(&self) -> Availability {
        self.inner.state.lock().unwrap().availability.clone()
    }
    pub fn set_availability(&self, availability: Availability) {
        let result = availability.command_result();
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.availability == availability {
                return;
            }
            state.availability = availability;
            state.revision = state.revision.wrapping_add(1);
        }
        self.inner.changed.notify(usize::MAX);
        if let Err(error) = result {
            for function in self.functions.iter() {
                let changed = {
                    let mut state = function.0.state.lock().unwrap();
                    let light = function.description().kind.is_light();
                    if let Some(commands) = state.commands_mut() {
                        commands.unavailable(light, error.clone());
                    }
                    if let Some(commands) = state.curtain_commands_mut() {
                        commands.unavailable(error.clone());
                        let changed = state.update_curtain_projection(|p| p.clear_pending_target());
                        function.notify_curtain_projection(changed);
                    }
                    if let Some(commands) = state.fan_commands_mut() {
                        commands.unavailable(error.clone());
                    }
                    if let Some(runtime) = state.heater_mut() {
                        runtime.thermostat.commands.unavailable(error.clone());
                    }
                    if let Some(runtime) = state.air_conditioner_mut() {
                        runtime.planning_mode = None;
                        runtime.observed_mode = None;
                        if runtime.dynamic_constraints {
                            runtime.constraints_current = false;
                        }
                        runtime.thermostat.commands.unavailable(error.clone());
                    }
                    if light {
                        function.notify_light_actions();
                    }
                    clear_light_state(function.description(), &mut state)
                };
                if changed {
                    function.0.changed.notify(usize::MAX);
                }
            }
        }
    }
    pub fn subscribe(&self) -> AvailabilityChanges {
        AvailabilityChanges {
            physical: self.inner.clone(),
            revision: self.inner.state.lock().unwrap().revision,
        }
    }
    /// A new feedback authority starts with unknown fields, without cancelling actions.
    pub(crate) fn clear_light_state(&self) {
        for function in self.functions.iter() {
            let changed = clear_light_state(
                function.description(),
                &mut function.0.state.lock().unwrap(),
            );
            if changed {
                function.0.changed.notify(usize::MAX);
            }
        }
    }

    /// Cancel light actions and unsent frames without closing live consumers.
    pub(crate) fn reset_actions(&self) {
        for function in self.functions.iter() {
            let mut state = function.0.state.lock().unwrap();
            if function.description().kind.is_light()
                && let Some(commands) = state.commands_mut()
            {
                commands.cancel_pending_light(super::CommandError::Cancelled);
                function.notify_light_actions();
            }
            if let Some(commands) = state.curtain_commands_mut() {
                commands.unavailable(super::CommandError::Cancelled);
                let changed = state.update_curtain_projection(|p| p.clear_pending_target());
                function.notify_curtain_projection(changed);
            }
            if let Some(commands) = state.fan_commands_mut() {
                commands.unavailable(super::CommandError::Cancelled);
            }
            if let Some(runtime) = state.heater_mut() {
                runtime
                    .thermostat
                    .commands
                    .unavailable(super::CommandError::Cancelled);
            }
            if let Some(runtime) = state.air_conditioner_mut() {
                runtime.planning_mode = None;
                runtime.observed_mode = None;
                runtime
                    .thermostat
                    .commands
                    .unavailable(super::CommandError::Cancelled);
            }
        }
    }

    pub(crate) fn close(&self) {
        for function in self.functions.iter() {
            function.close();
        }
    }
}
impl Function {
    fn new(
        physical_key: super::DeviceKey,
        description: FunctionDescription,
        physical: Arc<PhysicalInner>,
    ) -> (Self, Option<CommandReceiver>) {
        let key = FunctionKey::new(physical_key, description.id.clone());
        let state = RuntimeState::new(description.kind);
        let controllable = description.kind.is_control();
        let function = Self(Arc::new(Inner {
            description,
            key,
            physical,
            state: Mutex::new(state),
            changed: Event::new(),
            submitted: Event::new(),
            light_changed: Event::new(),
        }));
        if let super::FunctionKind::AirConditioner { temperature, .. } = function.description().kind
        {
            function.set_air_conditioner_constraints(
                super::air_conditioner::AirConditionerConstraints::standard(temperature),
            );
        }
        let receiver = controllable.then(|| CommandReceiver {
            function: function.clone(),
        });
        (function, receiver)
    }
    pub fn description(&self) -> &FunctionDescription {
        &self.0.description
    }
    pub fn function_key(&self) -> &FunctionKey {
        &self.0.key
    }
    pub fn snapshot(&self) -> FunctionSnapshot {
        let state = self.0.state.lock().unwrap();
        self.snapshot_locked(&state)
    }
    pub(super) fn snapshot_locked(&self, state: &RuntimeState) -> FunctionSnapshot {
        let physical = self.0.physical.state.lock().unwrap();
        let availability = physical.availability.clone();
        let revision = state.revision.wrapping_add(physical.revision);
        drop(physical);
        let ambient_temperature = state
            .air_conditioner()
            .map(|runtime| runtime.ambient_temperature());
        let temperature_source = state.temperature_source();
        let state = state.value.snapshot();
        FunctionSnapshot {
            state,
            availability,
            revision,
            temperature_source,
            ambient_temperature,
        }
    }
    /// Replace the associated ambient source after the configuration commits.
    pub(crate) fn set_ambient_temperature_source(
        &self,
        source: Option<(FunctionKey, Option<Function>)>,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state
            .air_conditioner_mut()
            .expect("air-conditioner function");
        let next = source.map(|(key, function)| AmbientSource { key, function });
        let same = match (&runtime.ambient_source, &next) {
            (None, None) => true,
            (Some(before), Some(after)) => {
                before.key == after.key
                    && match (&before.function, &after.function) {
                        (None, None) => true,
                        (Some(before), Some(after)) => Arc::ptr_eq(&before.0, &after.0),
                        _ => false,
                    }
            }
            _ => false,
        };
        if !same {
            runtime.ambient_source = next;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    fn ambient_source(&self) -> Option<Function> {
        let state = self.0.state.lock().unwrap();
        state
            .air_conditioner()
            .and_then(|runtime| runtime.ambient_source.as_ref())
            .and_then(|source| source.function.clone())
    }
    pub(super) fn close(&self) {
        if matches!(self.description().kind, super::FunctionKind::Temperature(_)) {
            self.set_temperature_source_available(false);
        }
        let mut state = self.0.state.lock().unwrap();
        let light = self.description().kind.is_light();
        if let Some(commands) = state.commands_mut() {
            commands.close(light, super::CommandError::Cancelled);
        }
        if let Some(commands) = state.curtain_commands_mut() {
            commands.close(super::CommandError::Cancelled);
        }
        if let Some(commands) = state.fan_commands_mut() {
            commands.close(super::CommandError::Cancelled);
        }
        if let Some(runtime) = state.heater_mut() {
            runtime
                .thermostat
                .commands
                .close(super::CommandError::Cancelled);
        }
        if let Some(runtime) = state.air_conditioner_mut() {
            runtime.planning_mode = None;
            runtime.observed_mode = None;
            runtime
                .thermostat
                .commands
                .close(super::CommandError::Cancelled);
        }
        if light {
            self.notify_light_actions();
        }
        self.0.submitted.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> FunctionChanges {
        let snapshot = self.snapshot();
        FunctionChanges {
            function: self.clone(),
            revision: snapshot.revision,
            ambient_temperature: snapshot.ambient_temperature,
        }
    }
}
/// Notifications can coalesce; consumers always read the same latest snapshot.
pub struct FunctionChanges {
    function: Function,
    revision: u64,
    ambient_temperature: Option<AmbientTemperature>,
}

/// One physical battery's independent state changes.
pub(crate) struct BatteryChanges {
    physical: Arc<PhysicalInner>,
    revision: u64,
}
impl BatteryChanges {
    pub(crate) async fn changed(&mut self) -> BatterySnapshot {
        loop {
            let listener = self.physical.battery_changed.listen();
            let current = {
                let battery = self.physical.battery.lock().unwrap();
                battery.as_ref().and_then(|battery| {
                    (battery.revision != self.revision)
                        .then_some((battery.revision, battery.snapshot))
                })
            };
            if let Some((revision, snapshot)) = current {
                self.revision = revision;
                return snapshot;
            }
            listener.await;
        }
    }
}

/// Physical-device observers are independent of function power changes.
pub struct AvailabilityChanges {
    physical: Arc<PhysicalInner>,
    revision: u64,
}
impl AvailabilityChanges {
    pub async fn changed(&mut self) -> Availability {
        loop {
            let listener = self.physical.changed.listen();
            {
                let state = self.physical.state.lock().unwrap();
                if state.revision != self.revision {
                    self.revision = state.revision;
                    return state.availability.clone();
                }
            }
            listener.await;
        }
    }
}
impl FunctionChanges {
    pub async fn changed(&mut self) -> FunctionSnapshot {
        loop {
            let listener = self.function.0.changed.listen();
            let physical = self.function.0.physical.changed.listen();
            let source = self.function.ambient_source();
            let source_listener = source.as_ref().map(|source| source.0.changed.listen());
            let snapshot = self.function.snapshot();
            if snapshot.revision != self.revision
                || snapshot.ambient_temperature != self.ambient_temperature
            {
                self.revision = snapshot.revision;
                self.ambient_temperature = snapshot.ambient_temperature;
                return snapshot;
            }
            let local = futures_lite::future::or(listener, physical);
            if let Some(source_listener) = source_listener {
                futures_lite::future::or(local, source_listener).await;
            } else {
                local.await;
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/identity.rs"]
mod test_identity;
