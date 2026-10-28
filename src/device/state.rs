use super::{
    command::{CommandCoordinator, CommandReceiver},
    model::{
        Availability, BatterySnapshot, FunctionDescription, FunctionKey, FunctionSnapshot,
        FunctionState, MeasurementSnapshot, PhysicalDeviceDescription, Power,
    },
};
use event_listener::Event;
use std::sync::{Arc, Mutex};
enum RuntimeValue {
    Control(Box<ControlRuntime>),
    Curtain(Box<CurtainRuntime>),
    Measurement(MeasurementSnapshot),
    StateSensor(super::StateSnapshot),
}
struct ControlRuntime {
    power: Power,
    lighting: Option<super::LightSnapshot>,
    commands: CommandCoordinator,
}
struct CurtainRuntime {
    snapshot: super::CurtainSnapshot,
    commands: super::curtain_command::CurtainCoordinator,
    projection: super::curtain_projection::CurtainProjection,
}
pub(super) struct RuntimeState {
    value: RuntimeValue,
    pub(super) revision: u64,
}
impl RuntimeState {
    fn curtain() -> Self {
        Self {
            value: RuntimeValue::Curtain(Box::new(CurtainRuntime {
                snapshot: super::CurtainSnapshot {
                    current_position: super::CurtainObservation {
                        value: None,
                        source_time: None,
                        received_at: None,
                    },
                    target_position: super::CurtainObservation {
                        value: None,
                        source_time: None,
                        received_at: None,
                    },
                    status: super::CurtainObservation {
                        value: None,
                        source_time: None,
                        received_at: None,
                    },
                },
                commands: super::curtain_command::CurtainCoordinator::default(),
                projection: super::curtain_projection::CurtainProjection::default(),
            })),
            revision: 0,
        }
    }
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
    fn measurement_state() -> Self {
        Self {
            value: RuntimeValue::Measurement(MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            }),
            revision: 0,
        }
    }
    fn state_sensor() -> Self {
        Self {
            value: RuntimeValue::StateSensor(super::StateSnapshot {
                active: None,
                input: None,
                source_time: None,
                received_at: None,
            }),
            revision: 0,
        }
    }
    fn has_control_state(&self) -> bool {
        matches!(
            self.value,
            RuntimeValue::Control(_) | RuntimeValue::Curtain(_)
        )
    }
    pub(super) fn curtain_commands(&self) -> Option<&super::curtain_command::CurtainCoordinator> {
        match &self.value {
            RuntimeValue::Curtain(curtain) => Some(&curtain.commands),
            _ => None,
        }
    }
    pub(super) fn curtain_commands_mut(
        &mut self,
    ) -> Option<&mut super::curtain_command::CurtainCoordinator> {
        match &mut self.value {
            RuntimeValue::Curtain(curtain) => Some(&mut curtain.commands),
            _ => None,
        }
    }
    pub(super) fn update_curtain_projection(
        &mut self,
        update: impl FnOnce(&mut super::curtain_projection::CurtainProjection),
    ) -> bool {
        let RuntimeValue::Curtain(curtain) = &mut self.value else {
            unreachable!("curtain function has curtain state")
        };
        let before = curtain.projection.target(curtain.snapshot);
        update(&mut curtain.projection);
        let changed = before != curtain.projection.target(curtain.snapshot);
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }
    pub(super) fn commands(&self) -> Option<&CommandCoordinator> {
        match &self.value {
            RuntimeValue::Control(control) => Some(&control.commands),
            RuntimeValue::Curtain(_)
            | RuntimeValue::Measurement(_)
            | RuntimeValue::StateSensor(_) => None,
        }
    }
    pub(super) fn commands_mut(&mut self) -> Option<&mut CommandCoordinator> {
        match &mut self.value {
            RuntimeValue::Control(control) => Some(&mut control.commands),
            RuntimeValue::Curtain(_)
            | RuntimeValue::Measurement(_)
            | RuntimeValue::StateSensor(_) => None,
        }
    }
    pub(super) fn power(&self) -> Option<Power> {
        match &self.value {
            RuntimeValue::Control(control) => Some(control.power),
            RuntimeValue::Curtain(_)
            | RuntimeValue::Measurement(_)
            | RuntimeValue::StateSensor(_) => None,
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
            RuntimeValue::Curtain(_)
            | RuntimeValue::Measurement(_)
            | RuntimeValue::StateSensor(_) => None,
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
            RuntimeValue::Curtain(_)
            | RuntimeValue::Measurement(_)
            | RuntimeValue::StateSensor(_) => None,
        }
    }
    fn measurement_mut(&mut self) -> Option<&mut MeasurementSnapshot> {
        match &mut self.value {
            RuntimeValue::Measurement(measurement) => Some(measurement),
            RuntimeValue::Control(_) | RuntimeValue::Curtain(_) | RuntimeValue::StateSensor(_) => {
                None
            }
        }
    }
}

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
        let state = if description.kind == super::FunctionKind::Curtain {
            RuntimeState::curtain()
        } else if description.kind.is_measurement() {
            RuntimeState::measurement_state()
        } else if description.kind.is_state_sensor() {
            RuntimeState::state_sensor()
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
        let state = match (&self.description().kind, &state.value) {
            (super::FunctionKind::Outlet, RuntimeValue::Control(control))
                if control.lighting.is_none() =>
            {
                FunctionState::Outlet {
                    power: control.power,
                }
            }
            (super::FunctionKind::Light(_), RuntimeValue::Control(control))
                if control.lighting.is_some() =>
            {
                FunctionState::Light {
                    power: control.power,
                    lighting: control.lighting.expect("light has lighting state"),
                }
            }
            (super::FunctionKind::Curtain, RuntimeValue::Curtain(curtain)) => {
                FunctionState::Curtain {
                    curtain: curtain.snapshot,
                }
            }
            (super::FunctionKind::Temperature(_), RuntimeValue::Measurement(measurement)) => {
                FunctionState::Temperature {
                    measurement: *measurement,
                }
            }
            (super::FunctionKind::Humidity(_), RuntimeValue::Measurement(measurement)) => {
                FunctionState::Humidity {
                    measurement: *measurement,
                }
            }
            (super::FunctionKind::CarbonDioxide(_), RuntimeValue::Measurement(measurement)) => {
                FunctionState::CarbonDioxide {
                    measurement: *measurement,
                }
            }
            (super::FunctionKind::Illuminance(_), RuntimeValue::Measurement(measurement)) => {
                FunctionState::Illuminance {
                    measurement: *measurement,
                }
            }
            (super::FunctionKind::Contact, RuntimeValue::StateSensor(state)) => {
                FunctionState::Contact {
                    state: state.clone(),
                }
            }
            (super::FunctionKind::Motion, RuntimeValue::StateSensor(state)) => {
                FunctionState::Motion {
                    state: state.clone(),
                }
            }
            (super::FunctionKind::Occupancy, RuntimeValue::StateSensor(state)) => {
                FunctionState::Occupancy {
                    state: state.clone(),
                }
            }
            _ => unreachable!("runtime value matches the function kind"),
        };
        FunctionSnapshot {
            state,
            availability,
            revision,
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
        if !self.description().is_measurement() {
            return Err(super::CommandError::Unsupported);
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
    pub(crate) fn matter_curtain(&self) -> (super::CurtainSnapshot, Option<u32>) {
        let state = self.0.state.lock().unwrap();
        let RuntimeValue::Curtain(curtain) = &state.value else {
            unreachable!("curtain function has curtain state")
        };
        (
            curtain.snapshot,
            curtain.projection.target(curtain.snapshot),
        )
    }
    pub(super) fn notify_curtain_projection(&self, changed: bool) {
        if changed {
            self.0.changed.notify(usize::MAX);
        }
    }
    /// A replacement source may have missed movement after the previous target.
    pub(crate) fn curtain_source_changed(&self) {
        let mut state = self.0.state.lock().unwrap();
        let changed = state.update_curtain_projection(|p| p.clear_pending_target());
        self.notify_curtain_projection(changed);
    }
    /// Apply one validated curtain report without conflating the three inputs.
    pub(crate) fn observe_curtain(
        &self,
        value: super::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_curtain(value, source_time, received_at, true);
    }
    /// Restore native fields without treating a cached target as a new command.
    pub(crate) fn read_curtain(
        &self,
        value: super::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_curtain(value, source_time, received_at, false);
    }
    fn apply_curtain(
        &self,
        value: super::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
        notification: bool,
    ) {
        fn update<T: Copy + PartialEq>(
            current: &mut super::CurtainObservation<T>,
            value: T,
            source_time: Option<i64>,
            received_at: i64,
        ) -> bool {
            let changed = current.value != Some(value);
            *current = super::CurtainObservation {
                value: Some(value),
                source_time,
                received_at: Some(received_at),
            };
            changed
        }
        let mut state = self.0.state.lock().unwrap();
        let RuntimeValue::Curtain(curtain) = &mut state.value else {
            unreachable!("curtain function has curtain state")
        };
        let before_target = curtain.projection.target(curtain.snapshot);
        curtain
            .projection
            .observe(&value, curtain.snapshot, notification);
        let changed = match value {
            super::CurtainValue::CurrentPosition(value) => update(
                &mut curtain.snapshot.current_position,
                value,
                source_time,
                received_at,
            ),
            super::CurtainValue::TargetPosition(value) => update(
                &mut curtain.snapshot.target_position,
                value,
                source_time,
                received_at,
            ),
            super::CurtainValue::Status(value) => update(
                &mut curtain.snapshot.status,
                value,
                source_time,
                received_at,
            ),
        };
        let projection_changed = before_target != curtain.projection.target(curtain.snapshot);
        if changed || projection_changed {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply one state observation already interpreted at the source boundary.
    pub fn observe_state(
        &self,
        active: Option<bool>,
        input: super::StateInput,
        source_time: Option<i64>,
        received_at: i64,
    ) -> Result<FunctionSnapshot, super::CommandError> {
        let mut state = self.0.state.lock().unwrap();
        let RuntimeValue::StateSensor(current) = &mut state.value else {
            return Err(super::CommandError::Unsupported);
        };
        let observed = super::StateSnapshot {
            active,
            input: Some(input),
            source_time,
            received_at: Some(received_at),
        };
        if *current != observed {
            *current = observed;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub fn observe(&self, power: bool) {
        if !matches!(
            self.description().kind,
            super::FunctionKind::Outlet | super::FunctionKind::Light(_)
        ) {
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
    /// Apply a complete logical observation without repeating its admission checks.
    pub(super) fn apply_light_observation(
        &self,
        observation: super::light_observation::LightObservation<'_>,
    ) -> FunctionSnapshot {
        let (patch, clear_temperature) = observation.into_parts();
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
        if clear_temperature {
            lighting.temperature_kelvin = None;
        }
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
        self.snapshot_locked(&state)
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
    pub(super) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        let light = self.description().kind.is_light();
        if let Some(commands) = state.commands_mut() {
            commands.close(light, super::CommandError::Cancelled);
        }
        if let Some(commands) = state.curtain_commands_mut() {
            commands.close(super::CommandError::Cancelled);
        }
        if light {
            self.notify_light_actions();
        }
        self.0.submitted.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> FunctionChanges {
        FunctionChanges {
            function: self.clone(),
            revision: self.snapshot().revision,
        }
    }
}
/// Notifications can coalesce; consumers always read the same latest snapshot.
pub struct FunctionChanges {
    function: Function,
    revision: u64,
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
            let snapshot = self.function.snapshot();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            futures_lite::future::or(listener, physical).await;
        }
    }
}

#[cfg(test)]
#[path = "tests/identity.rs"]
mod test_identity;
