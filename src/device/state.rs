use super::{
    command::{CommandReceiver, Queue},
    model::{
        Availability, Capability, Category, FunctionDescription, FunctionKey, FunctionSnapshot,
        LightMode, LightSnapshot, PhysicalDeviceDescription, Power,
    },
};
use event_listener::Event;
use std::sync::{Arc, Mutex};
/// Lighting state exists only for lights with at least one controllable light
/// dimension. A single color capability determines its mode; a dual
/// temperature/RGB light starts with an unknown mode until it is observed.
fn initial_lighting(description: &FunctionDescription) -> Option<LightSnapshot> {
    if description.category != Category::Light {
        return None;
    }
    let brightness = description.capabilities.contains(&Capability::Brightness);
    let temperature = description.temperature_range().is_some();
    let rgb = description.capabilities.contains(&Capability::ColorRgb);
    if !brightness && !temperature && !rgb {
        return None;
    }
    let mode = match (temperature, rgb) {
        (true, true) => None,
        (true, false) => Some(LightMode::Temperature),
        (false, true) => Some(LightMode::Rgb),
        (false, false) => None,
    };
    Some(LightSnapshot {
        brightness_percent: None,
        temperature_kelvin: None,
        color: None,
        mode,
    })
}

pub(super) struct RuntimeState {
    pub(super) power: Power,
    pub(super) lighting: Option<super::LightSnapshot>,
    pub(super) revision: u64,
    pub(super) queue: Queue,
    pub(super) light_commands: Option<super::light_command::LightCoordinator>,
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
        let (functions, receivers): (Vec<_>, Vec<_>) = description
            .functions
            .into_iter()
            .map(|function| Function::new(description.key.clone(), function, inner.clone()))
            .unzip();
        (
            Self {
                key: description.key,
                label: Arc::new(Mutex::new(description.label)),
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
    pub fn description(&self) -> PhysicalDeviceDescription {
        PhysicalDeviceDescription {
            key: self.key.clone(),
            label: self.label(),
            functions: self
                .functions
                .iter()
                .map(|function| function.description().clone())
                .collect(),
        }
    }
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
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
        if let Err(error) = result {
            for function in self.functions.iter() {
                let mut state = function.0.state.lock().unwrap();
                if function.description().category == Category::Light {
                    state.power = Power::Unknown;
                    state.lighting = initial_lighting(function.description());
                }
                if let Some(coordinator) = state.light_commands.as_mut() {
                    coordinator.cancel_all(error.clone());
                    state.queue.invalidate_light_power(error.clone());
                    function.notify_light_actions();
                }
                let completions = state.queue.drain_pending();
                for completion in completions {
                    completion.finish(Err(error.clone()));
                }
            }
        }
        self.inner.changed.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> AvailabilityChanges {
        AvailabilityChanges {
            physical: self.inner.clone(),
            revision: self.inner.state.lock().unwrap().revision,
        }
    }
    pub(super) fn close(&self) {
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
    ) -> (Self, CommandReceiver) {
        let key = FunctionKey::new(physical_key, description.id.clone());
        let lighting = initial_lighting(&description);
        let function = Self(Arc::new(Inner {
            description,
            key,
            physical,
            state: Mutex::new(RuntimeState {
                power: Power::Unknown,
                lighting,
                revision: 0,
                queue: Queue::default(),
                light_commands: lighting.map(|_| super::light_command::LightCoordinator::new()),
            }),
            changed: Event::new(),
            submitted: Event::new(),
            light_changed: Event::new(),
        }));
        let receiver = CommandReceiver {
            function: function.clone(),
        };
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
        FunctionSnapshot {
            power: state.power,
            lighting: state.lighting,
            availability: physical.availability.clone(),
            revision: state.revision.wrapping_add(physical.revision),
        }
    }
    /// Only reachable, open functions expose their observations. Structural
    /// single-color modes do not depend on observations.
    pub fn fresh_snapshot(&self) -> super::FreshSnapshot {
        let state = self.0.state.lock().unwrap();
        let mut snapshot = self.snapshot_locked(&state);
        let reachable = !state.queue.closed && snapshot.availability == Availability::Available;
        if !reachable {
            snapshot.power = Power::Unknown;
            snapshot.lighting = initial_lighting(self.description());
        }
        super::FreshSnapshot {
            key: self.0.key.clone(),
            snapshot,
            reachable,
        }
    }
    /// Apply one validated report. Omitted fields preserve their observations.
    /// Unsupported fields reject the whole report before any state changes.
    pub fn observe_light(
        &self,
        patch: super::LightPatch,
    ) -> Result<FunctionSnapshot, super::CommandError> {
        use super::CommandError;
        let description = self.description();
        if description.category != Category::Light {
            return Err(CommandError::Unsupported);
        }
        let has_on_off = description.capabilities.contains(&Capability::OnOff);
        let has_brightness = description.capabilities.contains(&Capability::Brightness);
        let has_rgb = description.capabilities.contains(&Capability::ColorRgb);
        let temperature_range = description.temperature_range();
        if !has_on_off && patch.power.is_some() {
            return Err(CommandError::Unsupported);
        }
        if !has_brightness && patch.brightness_percent.is_some() {
            return Err(CommandError::Unsupported);
        }
        if temperature_range.is_none() && patch.temperature_kelvin.is_some() {
            return Err(CommandError::Unsupported);
        }
        if !has_rgb && patch.color.is_some() {
            return Err(CommandError::Unsupported);
        }
        match patch.mode {
            Some(LightMode::Temperature) if temperature_range.is_none() => {
                return Err(CommandError::Unsupported);
            }
            Some(LightMode::Rgb) if !has_rgb => return Err(CommandError::Unsupported),
            _ => {}
        }
        if let Some(percent) = patch.brightness_percent
            && (!percent.is_finite() || !(0.0..=100.0).contains(&percent))
        {
            return Err(CommandError::Rejected(
                "brightness is outside 0..100 percent".into(),
            ));
        }
        if let Some(value) = patch.temperature_kelvin
            && !temperature_range.is_some_and(|range| range.contains(value))
        {
            return Err(CommandError::Rejected(
                "color temperature is outside its physical range".into(),
            ));
        }
        let mut state = self.0.state.lock().unwrap();
        let before = (state.power, state.lighting);
        let RuntimeState {
            power, lighting, ..
        } = &mut *state;
        if let Some(value) = patch.power {
            *power = value.into();
        }
        if let Some(lighting) = lighting.as_mut() {
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
        }
        if before != (*power, *lighting) {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub fn observe(&self, power: bool) {
        let mut state = self.0.state.lock().unwrap();
        self.update_power(&mut state, power);
    }
    pub(super) fn update_power(&self, state: &mut RuntimeState, power: bool) {
        let before = state.power;
        state.power = power.into();
        if before != state.power {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    pub(super) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        let was_closed = state.queue.closed;
        state.queue.closed = true;
        // Closing invalidates fresh snapshots without moving the content
        // revision. Old handles can never expose valid observations again.
        if !was_closed {
            self.0.changed.notify(usize::MAX);
        }
        let completions = if let Some(coordinator) = state.light_commands.as_mut() {
            coordinator.cancel_all(super::CommandError::Cancelled);
            state
                .queue
                .invalidate_light_power(super::CommandError::Cancelled);
            self.notify_light_actions();
            state.queue.drain_pending()
        } else {
            state.queue.drain_completions()
        };
        for completion in completions {
            completion.finish(Err(super::CommandError::Cancelled));
        }
        self.0.submitted.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> FunctionChanges {
        FunctionChanges {
            function: self.clone(),
            revision: self.snapshot().revision,
            closed: self.closed(),
        }
    }
    pub(super) fn closed(&self) -> bool {
        self.0.state.lock().unwrap().queue.closed
    }
}
/// Notifications can coalesce; consumers always read the same latest snapshot.
/// A close is a state change even though it leaves the content revision alone.
pub struct FunctionChanges {
    function: Function,
    revision: u64,
    closed: bool,
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
            let closed = self.function.closed();
            if snapshot.revision != self.revision || closed != self.closed {
                self.revision = snapshot.revision;
                self.closed = closed;
                return snapshot;
            }
            futures_lite::future::or(listener, physical).await;
        }
    }
}
