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
    fresh: Freshness,
    pub(super) revision: u64,
    pub(super) queue: Queue,
    pub(super) light_commands: Option<super::light_command::LightCoordinator>,
}

/// Which function field an observation belongs to.
#[derive(Clone, Copy)]
enum Field {
    Power,
    Brightness,
    Temperature,
    Color,
    Mode,
}
impl Field {
    fn index(self) -> usize {
        self as usize
    }
}

/// Per-field observation validity since the last reachability loss. A value
/// whose validity is cleared may still live in the single-light cache, but it
/// must not participate in group aggregation.
#[derive(Clone, Copy, Eq, PartialEq)]
struct Freshness {
    valid: [bool; 5],
}
impl Freshness {
    fn new(structural_mode: bool) -> Self {
        Self {
            valid: [false, false, false, false, structural_mode],
        }
    }
    fn observe(&mut self, field: Field) {
        self.valid[field.index()] = true;
    }
    fn is_valid(self, field: Field) -> bool {
        self.valid[field.index()]
    }
    fn invalidate(&mut self, structural_mode: bool) {
        *self = Self::new(structural_mode);
    }
}

/// A single color kind fixes the active mode, so it stays valid without a mode
/// property; a dual-color light only knows its mode after an observation.
fn structural_mode(description: &FunctionDescription) -> bool {
    description.temperature_range().is_some()
        != description.capabilities.contains(&Capability::ColorRgb)
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
        self.inner.changed.notify(usize::MAX);
        if result.is_err() {
            for function in self.functions.iter() {
                function.invalidate_freshness();
            }
        }
        if let Err(error) = result {
            for function in self.functions.iter() {
                let mut state = function.0.state.lock().unwrap();
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
        let fresh = Freshness::new(structural_mode(&description));
        let function = Self(Arc::new(Inner {
            description,
            key,
            physical,
            state: Mutex::new(RuntimeState {
                power: Power::Unknown,
                lighting,
                fresh,
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
    /// A snapshot filtered to still-valid observations. Reachability loss and
    /// close always yield unknown fields even when the single-light cache and
    /// an old handle still carry values.
    pub fn fresh_snapshot(&self) -> super::FreshSnapshot {
        let state = self.0.state.lock().unwrap();
        let snapshot = self.snapshot_locked(&state);
        let reachable = !state.queue.closed && snapshot.availability == Availability::Available;
        let fresh = |field: Field| reachable && state.fresh.is_valid(field);
        super::FreshSnapshot {
            key: self.0.key.clone(),
            snapshot: FunctionSnapshot {
                power: if fresh(Field::Power) && snapshot.power != Power::Unknown {
                    snapshot.power
                } else {
                    Power::Unknown
                },
                lighting: snapshot.lighting.map(|lighting| LightSnapshot {
                    brightness_percent: if fresh(Field::Brightness) {
                        lighting.brightness_percent
                    } else {
                        None
                    },
                    temperature_kelvin: if fresh(Field::Temperature) {
                        lighting.temperature_kelvin
                    } else {
                        None
                    },
                    color: if fresh(Field::Color) {
                        lighting.color
                    } else {
                        None
                    },
                    mode: if fresh(Field::Mode) {
                        lighting.mode
                    } else {
                        None
                    },
                }),
                availability: snapshot.availability,
                revision: snapshot.revision,
            },
            reachable,
        }
    }
    /// Apply one validated report. Omitted fields preserve their observations;
    /// received fields become fresh even when their cached value is unchanged.
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
        let before = (state.power, state.lighting, state.fresh);
        let RuntimeState {
            power,
            lighting,
            fresh,
            ..
        } = &mut *state;
        if let Some(value) = patch.power {
            *power = value.into();
            fresh.observe(Field::Power);
        }
        if let Some(lighting) = lighting.as_mut() {
            if let Some(value) = patch.brightness_percent {
                lighting.brightness_percent = Some(value);
                fresh.observe(Field::Brightness);
            }
            if let Some(value) = patch.temperature_kelvin {
                lighting.temperature_kelvin = Some(value);
                fresh.observe(Field::Temperature);
            }
            if let Some(value) = patch.color {
                lighting.color = Some(value);
                fresh.observe(Field::Color);
            }
            if let Some(value) = patch.mode {
                lighting.mode = Some(value);
                fresh.observe(Field::Mode);
            }
        }
        if before != (*power, *lighting, *fresh) {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub(super) fn invalidate_freshness(&self) {
        let structural = structural_mode(&self.0.description);
        self.0.state.lock().unwrap().fresh.invalidate(structural);
    }
    pub fn observe(&self, power: bool) {
        let mut state = self.0.state.lock().unwrap();
        self.update_power(&mut state, power);
    }
    pub(super) fn update_power(&self, state: &mut RuntimeState, power: bool) {
        let before = (state.power, state.fresh);
        state.fresh.observe(Field::Power);
        state.power = power.into();
        if before != (state.power, state.fresh) {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    pub(super) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        let was_closed = state.queue.closed;
        state.queue.closed = true;
        // Closing removes every fresh observation. Subscribers are woken through
        // the shared event without moving the revision, so cached snapshots stay
        // comparable while closed members can never look valid again.
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
