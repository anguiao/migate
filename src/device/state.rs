use super::{
    command::{CommandReceiver, Queue},
    model::{
        Availability, DeviceDescription, DeviceSnapshot, FunctionKey, PhysicalDeviceDescription,
        Power,
    },
};
use event_listener::Event;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn wait_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            async_io::Timer::at(self.0 + deadline).await;
        })
    }
}

pub(super) struct RuntimeState {
    pub(super) power: Power,
    pub(super) revision: u64,
    pub(super) queue: Queue,
}
struct PhysicalState {
    availability: Availability,
    revision: u64,
}
struct PhysicalInner {
    state: Mutex<PhysicalState>,
    functions: Mutex<Vec<Weak<Inner>>>,
    changed: Event,
}
pub(super) struct Inner {
    description: DeviceDescription,
    key: FunctionKey,
    physical: Arc<PhysicalInner>,
    pub(super) state: Mutex<RuntimeState>,
    changed: Event,
    pub(super) submitted: Event,
    pub(super) clock: Box<dyn Clock>,
}

/// A channel's shared state and command boundary for every protocol adapter.
#[derive(Clone)]
pub struct Device(pub(super) Arc<Inner>);
pub type Function = Device;

/// Availability belongs to the physical device; each function has its own state and queue.
#[derive(Clone)]
pub struct PhysicalDevice {
    description: Arc<PhysicalDeviceDescription>,
    inner: Arc<PhysicalInner>,
    functions: Vec<Function>,
}
impl PhysicalDevice {
    pub fn new(description: PhysicalDeviceDescription) -> (Self, Vec<CommandReceiver>) {
        let inner = new_physical();
        let mut functions = Vec::new();
        let mut receivers = Vec::new();
        for function in &description.functions {
            let (device, receiver) = Device::with_physical(
                DeviceDescription {
                    key: description.key.clone(),
                    category: function.category,
                    capabilities: function.capabilities.clone(),
                },
                function.id.clone(),
                inner.clone(),
                Box::new(SystemClock(Instant::now())),
            );
            functions.push(device);
            receivers.push(receiver);
        }
        (
            Self {
                description: Arc::new(description),
                inner,
                functions,
            },
            receivers,
        )
    }
    pub fn description(&self) -> &PhysicalDeviceDescription {
        &self.description
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
        set_availability(&self.inner, availability);
    }
    pub(super) fn close(&self) {
        for function in &self.functions {
            function.close();
        }
    }
}
fn new_physical() -> Arc<PhysicalInner> {
    Arc::new(PhysicalInner {
        state: Mutex::new(PhysicalState {
            availability: Availability::Initializing,
            revision: 0,
        }),
        functions: Mutex::new(Vec::new()),
        changed: Event::new(),
    })
}
fn set_availability(physical: &PhysicalInner, availability: Availability) {
    let result = availability.command_result();
    {
        let mut state = physical.state.lock().unwrap();
        if state.availability == availability {
            return;
        }
        state.availability = availability;
        state.revision = state.revision.wrapping_add(1);
    }
    physical.changed.notify(usize::MAX);
    if let Err(error) = result {
        for function in physical
            .functions
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            let completions = function.state.lock().unwrap().queue.drain_completions();
            for completion in completions {
                completion.finish(Err(error.clone()));
            }
        }
    }
}
impl Device {
    pub fn new(description: DeviceDescription) -> (Self, CommandReceiver) {
        Self::with_physical(
            description,
            "main".into(),
            new_physical(),
            Box::new(SystemClock(Instant::now())),
        )
    }
    #[cfg(test)]
    pub(crate) fn for_test(
        description: DeviceDescription,
        clock: Box<dyn Clock>,
    ) -> (Self, CommandReceiver) {
        Self::with_physical(description, "main".into(), new_physical(), clock)
    }
    fn with_physical(
        description: DeviceDescription,
        id: String,
        physical: Arc<PhysicalInner>,
        clock: Box<dyn Clock>,
    ) -> (Self, CommandReceiver) {
        let key = FunctionKey::new(description.key.clone(), id);
        let device = Self(Arc::new(Inner {
            description,
            key,
            physical: physical.clone(),
            state: Mutex::new(RuntimeState {
                power: Power::Unknown,
                revision: 0,
                queue: Queue::default(),
            }),
            changed: Event::new(),
            submitted: Event::new(),
            clock,
        }));
        physical
            .functions
            .lock()
            .unwrap()
            .push(Arc::downgrade(&device.0));
        let receiver = CommandReceiver {
            device: device.clone(),
        };
        (device, receiver)
    }
    pub fn description(&self) -> &DeviceDescription {
        &self.0.description
    }
    pub fn function_key(&self) -> &FunctionKey {
        &self.0.key
    }
    pub fn snapshot(&self) -> DeviceSnapshot {
        let state = self.0.state.lock().unwrap();
        self.snapshot_locked(&state)
    }
    pub(super) fn snapshot_locked(&self, state: &RuntimeState) -> DeviceSnapshot {
        let physical = self.0.physical.state.lock().unwrap();
        DeviceSnapshot {
            power: state.power,
            availability: physical.availability.clone(),
            revision: state.revision.wrapping_add(physical.revision),
        }
    }
    pub fn observe(&self, power: bool) {
        let mut state = self.0.state.lock().unwrap();
        self.update_power(&mut state, power);
    }
    pub fn set_availability(&self, availability: Availability) {
        set_availability(&self.0.physical, availability);
    }
    pub(super) fn update_power(&self, state: &mut RuntimeState, power: bool) -> DeviceSnapshot {
        let power = power.into();
        if state.power != power {
            state.power = power;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        self.snapshot_locked(state)
    }
    pub(super) fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.queue.closed = true;
        for completion in state.queue.drain_completions() {
            completion.finish(Err(super::CommandError::Cancelled));
        }
        self.0.submitted.notify(usize::MAX);
    }
    pub fn subscribe(&self) -> Changes {
        Changes {
            device: self.clone(),
            revision: self.snapshot().revision,
        }
    }
}
/// Notifications can coalesce; consumers always read the same latest snapshot.
pub struct Changes {
    device: Device,
    revision: u64,
}
impl Changes {
    pub async fn changed(&mut self) -> DeviceSnapshot {
        loop {
            let listener = self.device.0.changed.listen();
            let physical = self.device.0.physical.changed.listen();
            let snapshot = self.device.snapshot();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            futures_lite::future::or(listener, physical).await;
        }
    }
}
