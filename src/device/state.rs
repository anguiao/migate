use super::{
    command::{CommandCoordinator, CommandReceiver},
    model::{
        Availability, FunctionDescription, FunctionKey, FunctionSnapshot, MeasurementSnapshot,
        PhysicalDeviceDescription, Power,
    },
};
use event_listener::Event;
use std::sync::{Arc, Mutex};
enum RuntimeValue {
    Control(Box<ControlRuntime>),
    Temperature(MeasurementSnapshot),
    Humidity(MeasurementSnapshot),
}
struct ControlRuntime {
    power: Power,
    lighting: Option<super::LightSnapshot>,
    commands: CommandCoordinator,
}
pub(super) struct RuntimeState {
    value: RuntimeValue,
    pub(super) revision: u64,
}
impl RuntimeState {
    fn control(lighting: Option<super::LightSnapshot>) -> Self {
        Self {
            value: RuntimeValue::Control(Box::new(ControlRuntime {
                power: Power::Unknown,
                lighting,
                commands: CommandCoordinator::new(),
            })),
            revision: 0,
        }
    }
    fn measurement_state(kind: super::FunctionKind) -> Self {
        let measurement = MeasurementSnapshot {
            value: None,
            source_time: None,
            received_at: None,
        };
        let value = match kind {
            super::FunctionKind::Temperature(_) => RuntimeValue::Temperature(measurement),
            super::FunctionKind::Humidity(_) => RuntimeValue::Humidity(measurement),
            super::FunctionKind::Outlet | super::FunctionKind::Light(_) => {
                unreachable!("measurement runtime kind")
            }
        };
        Self { value, revision: 0 }
    }
    fn has_control_state(&self) -> bool {
        matches!(self.value, RuntimeValue::Control(_))
    }
    pub(super) fn commands(&self) -> Option<&CommandCoordinator> {
        match &self.value {
            RuntimeValue::Control(control) => Some(&control.commands),
            RuntimeValue::Temperature(_) | RuntimeValue::Humidity(_) => None,
        }
    }
    pub(super) fn commands_mut(&mut self) -> Option<&mut CommandCoordinator> {
        match &mut self.value {
            RuntimeValue::Control(control) => Some(&mut control.commands),
            RuntimeValue::Temperature(_) | RuntimeValue::Humidity(_) => None,
        }
    }
    pub(super) fn power(&self) -> Option<Power> {
        match &self.value {
            RuntimeValue::Control(control) => Some(control.power),
            RuntimeValue::Temperature(_) | RuntimeValue::Humidity(_) => None,
        }
    }
    fn set_power(&mut self, new: Power) {
        if let RuntimeValue::Control(control) = &mut self.value {
            control.power = new;
        }
    }
    fn control_parts(&self) -> Option<(Power, Option<super::LightSnapshot>)> {
        match &self.value {
            RuntimeValue::Control(control) => Some((control.power, control.lighting)),
            RuntimeValue::Temperature(_) | RuntimeValue::Humidity(_) => None,
        }
    }
    fn set_control_parts(&mut self, power: Power, lighting: Option<super::LightSnapshot>) {
        if let RuntimeValue::Control(control) = &mut self.value {
            control.power = power;
            control.lighting = lighting;
        }
    }
    fn lighting_mut(&mut self) -> Option<&mut super::LightSnapshot> {
        match &mut self.value {
            RuntimeValue::Control(control) => control.lighting.as_mut(),
            RuntimeValue::Temperature(_) | RuntimeValue::Humidity(_) => None,
        }
    }
    fn measurement_mut(&mut self) -> Option<&mut MeasurementSnapshot> {
        match &mut self.value {
            RuntimeValue::Temperature(measurement) | RuntimeValue::Humidity(measurement) => {
                Some(measurement)
            }
            RuntimeValue::Control(_) => None,
        }
    }
}

struct PhysicalState {
    availability: Availability,
    revision: u64,
}
struct PhysicalInner {
    state: Mutex<PhysicalState>,
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
    let Some((power, Some(before_lighting))) = state.control_parts() else {
        return false;
    };
    let mut lighting = before_lighting;
    lighting.brightness_percent = None;
    lighting.temperature_kelvin = None;
    lighting.color = None;
    lighting.mode = description.inferred_color_mode();
    let before = (power, Some(before_lighting));
    let after = (Power::Unknown, Some(lighting));
    if before != after {
        state.set_control_parts(after.0, after.1);
        state.revision = state.revision.wrapping_add(1);
        true
    } else {
        false
    }
}

/// A channel's shared state and command boundary for every protocol adapter.
#[derive(Clone)]
pub struct Function(pub(super) Arc<Inner>);

/// Availability belongs to the physical device; each function has its own state and queue.
#[derive(Clone)]
pub struct PhysicalDevice {
    key: super::DeviceKey,
    label: Arc<Mutex<String>>,
    inner: Arc<PhysicalInner>,
    functions: Arc<[Function]>,
}
impl PhysicalDevice {
    pub fn new(description: PhysicalDeviceDescription) -> (Self, Vec<CommandReceiver>) {
        let inner = Arc::new(PhysicalInner {
            state: Mutex::new(PhysicalState {
                availability: Availability::Initializing,
                revision: 0,
            }),
            changed: Event::new(),
        });
        let (key, label, functions) = description.into_parts();
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
        let lighting = description.kind.is_light().then_some(super::LightSnapshot {
            brightness_percent: None,
            temperature_kelvin: None,
            color: None,
            mode: description.inferred_color_mode(),
        });
        let state = if description.kind.is_measurement() {
            RuntimeState::measurement_state(description.kind)
        } else {
            RuntimeState::control(lighting)
        };
        let controllable = state.has_control_state();
        let function = Self(Arc::new(Inner {
            description,
            key,
            physical,
            state: Mutex::new(state),
            changed: Event::new(),
            submitted: Event::new(),
            light_changed: Event::new(),
        }));
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
        match (&self.description().kind, &state.value) {
            (super::FunctionKind::Outlet, RuntimeValue::Control(control))
                if control.lighting.is_none() =>
            {
                FunctionSnapshot::Outlet {
                    power: control.power,
                    availability,
                    revision,
                }
            }
            (super::FunctionKind::Light(_), RuntimeValue::Control(control))
                if control.lighting.is_some() =>
            {
                FunctionSnapshot::Light {
                    power: control.power,
                    lighting: control.lighting.expect("light has lighting state"),
                    availability,
                    revision,
                }
            }
            (super::FunctionKind::Temperature(_), RuntimeValue::Temperature(measurement)) => {
                FunctionSnapshot::Temperature {
                    measurement: *measurement,
                    availability,
                    revision,
                }
            }
            (super::FunctionKind::Humidity(_), RuntimeValue::Humidity(measurement)) => {
                FunctionSnapshot::Humidity {
                    measurement: *measurement,
                    availability,
                    revision,
                }
            }
            _ => unreachable!("runtime value matches the function kind"),
        }
    }
    /// Apply one validated measurement. Missing source time stays unknown for
    /// this observation instead of inheriting an earlier timestamp.
    pub fn observe_measurement(
        &self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> Result<FunctionSnapshot, super::CommandError> {
        let Some(range) = self.description().measurement_range() else {
            return Err(super::CommandError::Unsupported);
        };
        if !range.contains(value) {
            return Err(super::CommandError::Rejected("measurement".into()));
        }
        let observed = MeasurementSnapshot {
            value: Some(value),
            source_time,
            received_at: Some(received_at),
        };
        let mut state = self.0.state.lock().unwrap();
        let current = state
            .measurement_mut()
            .expect("measurement function has measurement state");
        if *current != observed {
            *current = observed;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub fn observe(&self, power: bool) {
        if self.description().is_measurement() {
            return;
        }
        let mut state = self.0.state.lock().unwrap();
        self.update_power(&mut state, power);
    }
    pub(super) fn update_power(&self, state: &mut RuntimeState, power: bool) {
        let power = power.into();
        if state.power() != Some(power) {
            state.set_power(power);
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply one validated function report and publish one shared revision.
    pub fn observe_light(
        &self,
        patch: super::LightPatch,
    ) -> Result<FunctionSnapshot, super::CommandError> {
        use super::{CommandError, LightMode};
        let description = self.description();
        let Some(profile) = description.kind.light_profile() else {
            return Err(CommandError::Unsupported);
        };
        if patch.brightness_percent.is_some() && !profile.has_brightness() {
            return Err(CommandError::Unsupported);
        }
        if let Some(percent) = patch.brightness_percent
            && (!percent.is_finite() || !(0.0..=100.0).contains(&percent))
        {
            return Err(CommandError::Rejected(
                "brightness is outside 0..100 percent".into(),
            ));
        }
        if let Some(value) = patch.temperature_kelvin {
            let Some(interval) = profile.temperature_interval() else {
                return Err(CommandError::Unsupported);
            };
            if !interval.contains(value) {
                return Err(CommandError::Rejected(
                    "color temperature is outside its physical range".into(),
                ));
            }
        }
        if patch.mode == Some(LightMode::Temperature) && profile.temperature_interval().is_none() {
            return Err(CommandError::Unsupported);
        }
        if !profile.has_rgb() && (patch.color.is_some() || patch.mode == Some(LightMode::Rgb)) {
            return Err(CommandError::Unsupported);
        }
        let mut state = self.0.state.lock().unwrap();
        let (before_power, Some(before_lighting)) =
            state.control_parts().expect("light has control state")
        else {
            unreachable!("light has lighting state")
        };
        let mut power = before_power;
        if let Some(value) = patch.power {
            power = value.into();
        }
        let mut lighting = before_lighting;
        if let Some(value) = patch.brightness_percent {
            lighting.brightness_percent = Some(value);
        }
        if let Some(value) = patch.temperature_kelvin {
            lighting.temperature_kelvin = Some(value);
        }
        if let Some(value) = patch.color {
            lighting.color = Some(value);
        }
        if let Some(value) = patch.mode {
            lighting.mode = Some(value);
        }
        if (before_power, Some(before_lighting)) != (power, Some(lighting)) {
            state.set_control_parts(power, Some(lighting));
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    /// Replace the represented RGB and mode, including unknown fields.
    pub(crate) fn replace_light_color(
        &self,
        color: Option<super::Rgb>,
        mode: Option<super::LightMode>,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let light = state.lighting_mut().expect("light has lighting state");
        if (light.color, light.mode) != (color, mode) {
            light.color = color;
            light.mode = mode;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Replace a derived light snapshot, including fields that became unknown.
    pub(crate) fn replace_light_state(&self, power: Power, lighting: super::LightSnapshot) {
        let mut state = self.0.state.lock().unwrap();
        if state.control_parts() != Some((power, Some(lighting))) {
            state.set_control_parts(power, Some(lighting));
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Clear an out-of-publication color temperature to unknown.
    pub(crate) fn clear_light_temperature(&self) -> FunctionSnapshot {
        let changed = {
            let mut state = self.0.state.lock().unwrap();
            let before = state.control_parts();
            if let Some(lighting) = state.lighting_mut() {
                lighting.temperature_kelvin = None;
            }
            let changed = before != state.control_parts();
            if changed {
                state.revision = state.revision.wrapping_add(1);
            }
            changed
        };
        if changed {
            self.0.changed.notify(usize::MAX);
        }
        self.snapshot()
    }
    pub(super) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        let light = self.description().kind.is_light();
        if let Some(commands) = state.commands_mut() {
            commands.close(light, super::CommandError::Cancelled);
        }
        if light {
            self.notify_light_actions();
        }
        self.0.submitted.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> FunctionChanges {
        FunctionChanges {
            function: self.clone(),
            revision: self.snapshot().revision(),
        }
    }
}
/// Notifications can coalesce; consumers always read the same latest snapshot.
pub struct FunctionChanges {
    function: Function,
    revision: u64,
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
            let snapshot = self.function.snapshot();
            if snapshot.revision() != self.revision {
                self.revision = snapshot.revision();
                return snapshot;
            }
            futures_lite::future::or(listener, physical).await;
        }
    }
}

#[cfg(test)]
#[path = "tests/identity.rs"]
mod test_identity;
