use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use event_listener::Event;

pub const VIRTUAL_LIGHT_ID: &str = "virtual-light-1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub id: &'static str,
    pub power: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceKey {
    pub source: String,
    pub id: String,
}

impl DeviceKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Category {
    Outlet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    OnOff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDescription {
    pub key: DeviceKey,
    pub category: Category,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Power {
    Unknown,
    On,
    Off,
}

impl Power {
    pub fn value(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }
}

impl From<bool> for Power {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    Initializing,
    Available,
    Unavailable(String),
}

impl Availability {
    fn command_result(&self) -> Result<(), CommandError> {
        match self {
            Self::Available => Ok(()),
            Self::Initializing => Err(CommandError::Unavailable("initializing".into())),
            Self::Unavailable(reason) => Err(CommandError::Unavailable(reason.clone())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSnapshot {
    pub power: Power,
    pub availability: Availability,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unsupported,
    Unavailable(String),
    UnknownState,
    Timeout,
    Rejected(String),
    SendFailed(String),
    Cancelled,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("device does not support on/off commands"),
            Self::Unavailable(reason) => write!(formatter, "device unavailable: {reason}"),
            Self::UnknownState => formatter.write_str("cannot toggle an unknown power state"),
            Self::Timeout => formatter.write_str("command acceptance timed out"),
            Self::Rejected(reason) => write!(formatter, "command rejected: {reason}"),
            Self::SendFailed(reason) => write!(formatter, "command send failed: {reason}"),
            Self::Cancelled => formatter.write_str("command cancelled"),
        }
    }
}

impl Error for CommandError {}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

trait Clock: Send + Sync {
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

#[derive(Default)]
struct Queue {
    pending: VecDeque<PendingCommand>,
    active: Option<Arc<Completion>>,
    closed: bool,
}

impl Queue {
    fn drain_completions(&mut self) -> Vec<Arc<Completion>> {
        let active = self.active.take();
        self.pending
            .drain(..)
            .map(|pending| pending.completion)
            .chain(active)
            .collect()
    }
}

struct RuntimeState {
    snapshot: DeviceSnapshot,
    queue: Queue,
}

struct Inner {
    description: DeviceDescription,
    state: Mutex<RuntimeState>,
    changed: Event,
    submitted: Event,
    clock: Box<dyn Clock>,
}

/// One shared state and command boundary for every protocol adapter.
#[derive(Clone)]
pub struct Device(Arc<Inner>);

impl Device {
    pub fn new(description: DeviceDescription) -> (Self, CommandReceiver) {
        Self::with_clock(description, Box::new(SystemClock(Instant::now())))
    }

    fn with_clock(
        description: DeviceDescription,
        clock: Box<dyn Clock>,
    ) -> (Self, CommandReceiver) {
        let device = Self(Arc::new(Inner {
            description,
            state: Mutex::new(RuntimeState {
                snapshot: DeviceSnapshot {
                    power: Power::Unknown,
                    availability: Availability::Initializing,
                    revision: 0,
                },
                queue: Queue::default(),
            }),
            changed: Event::new(),
            submitted: Event::new(),
            clock,
        }));
        let receiver = CommandReceiver {
            device: device.clone(),
        };
        (device, receiver)
    }

    pub fn description(&self) -> &DeviceDescription {
        &self.0.description
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        self.0.state.lock().unwrap().snapshot.clone()
    }

    pub fn observe(&self, power: bool) {
        self.update(|snapshot| snapshot.power = power.into());
    }

    pub fn set_availability(&self, availability: Availability) {
        let command_result = availability.command_result();
        let mut state = self.0.state.lock().unwrap();
        self.update_snapshot(&mut state.snapshot, |snapshot| {
            snapshot.availability = availability
        });
        if let Err(error) = command_result {
            let completions = state.queue.drain_completions();
            for completion in completions {
                completion.finish(Err(error.clone()));
            }
        }
    }

    fn update(&self, update: impl FnOnce(&mut DeviceSnapshot)) -> DeviceSnapshot {
        let mut state = self.0.state.lock().unwrap();
        self.update_snapshot(&mut state.snapshot, update)
    }

    fn update_snapshot(
        &self,
        snapshot: &mut DeviceSnapshot,
        update: impl FnOnce(&mut DeviceSnapshot),
    ) -> DeviceSnapshot {
        let before = snapshot.clone();
        update(snapshot);
        if *snapshot != before {
            snapshot.revision = snapshot.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        snapshot.clone()
    }

    pub fn subscribe(&self) -> Changes {
        Changes {
            device: self.clone(),
            revision: self.snapshot().revision,
        }
    }

    /// Completion means the downstream adapter accepted the command, not that
    /// the physical action has been independently confirmed.
    pub async fn execute(&self, command: Command) -> Result<DeviceSnapshot, CommandError> {
        let deadline = self.0.clock.now() + COMMAND_TIMEOUT;
        if !self.description().capabilities.contains(&Capability::OnOff) {
            return Err(CommandError::Unsupported);
        }
        let completion = Arc::new(Completion::default());
        {
            let mut state = self.0.state.lock().unwrap();
            state.snapshot.availability.command_result()?;
            if state.queue.closed {
                return Err(CommandError::Cancelled);
            }
            state.queue.pending.push_back(PendingCommand {
                command,
                deadline,
                completion: completion.clone(),
            });
        }
        self.0.submitted.notify(usize::MAX);
        futures_lite::future::race(completion.wait(), async {
            self.0.clock.wait_until(deadline).await;
            completion.finish(Err(CommandError::Timeout));
            completion.wait().await
        })
        .await
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
            let snapshot = self.device.snapshot();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            listener.await;
        }
    }
}

#[derive(Default)]
struct Completion {
    result: Mutex<Option<Result<DeviceSnapshot, CommandError>>>,
    changed: Event,
}

impl Completion {
    fn finish(&self, result: Result<DeviceSnapshot, CommandError>) {
        let mut current = self.result.lock().unwrap();
        if current.is_none() {
            *current = Some(result);
            self.changed.notify(usize::MAX);
        }
    }

    async fn wait(&self) -> Result<DeviceSnapshot, CommandError> {
        loop {
            let listener = self.changed.listen();
            if let Some(result) = self.result.lock().unwrap().clone() {
                return result;
            }
            listener.await;
        }
    }
}

struct PendingCommand {
    command: Command,
    deadline: Duration,
    completion: Arc<Completion>,
}

/// The single command consumer preserves receive order across adapter sessions.
pub struct CommandReceiver {
    device: Device,
}

impl CommandReceiver {
    pub async fn next(&mut self) -> Option<CommandRequest> {
        let active = self.device.0.state.lock().unwrap().queue.active.clone();
        if let Some(active) = active {
            let _ = active.wait().await;
            self.device.0.state.lock().unwrap().queue.active = None;
        }
        loop {
            let listener = self.device.0.submitted.listen();
            let request = {
                let mut state = self.device.0.state.lock().unwrap();
                if let Some(pending) = state.queue.pending.pop_front() {
                    if pending.deadline <= self.device.0.clock.now() {
                        pending.completion.finish(Err(CommandError::Timeout));
                        continue;
                    }
                    let snapshot = &state.snapshot;
                    let target =
                        snapshot.availability.command_result().and_then(|()| {
                            match pending.command {
                                Command::On => Ok(true),
                                Command::Off => Ok(false),
                                Command::Toggle => snapshot
                                    .power
                                    .value()
                                    .map(|value| !value)
                                    .ok_or(CommandError::UnknownState),
                            }
                        });
                    let target = match target {
                        Ok(target) => target,
                        Err(error) => {
                            pending.completion.finish(Err(error));
                            continue;
                        }
                    };
                    state.queue.active = Some(pending.completion.clone());
                    Some(CommandRequest {
                        target,
                        device: self.device.clone(),
                        deadline: pending.deadline,
                        completion: pending.completion,
                    })
                } else {
                    None
                }
            };
            if let Some(request) = request {
                return Some(request);
            }
            listener.await;
        }
    }
}

impl Drop for CommandReceiver {
    fn drop(&mut self) {
        let mut state = self.device.0.state.lock().unwrap();
        state.queue.closed = true;
        let completions = state.queue.drain_completions();
        for completion in completions {
            completion.finish(Err(CommandError::Cancelled));
        }
    }
}

/// A protocol-independent absolute write. The adapter must use `remaining()`
/// when bounding its request and must not send once that duration is zero.
pub struct CommandRequest {
    pub target: bool,
    device: Device,
    deadline: Duration,
    completion: Arc<Completion>,
}

impl CommandRequest {
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_sub(self.device.0.clock.now())
    }

    pub fn accept(self) -> Result<DeviceSnapshot, CommandError> {
        let mut state = self.device.0.state.lock().unwrap();
        let mut result = self.completion.result.lock().unwrap();
        if let Some(completed) = &*result {
            return completed.clone();
        }
        let accepted = if self.remaining().is_zero() {
            Err(CommandError::Timeout)
        } else {
            state.snapshot.availability.command_result().map(|()| {
                self.device
                    .update_snapshot(&mut state.snapshot, |snapshot| {
                        snapshot.power = self.target.into()
                    })
            })
        };
        *result = Some(accepted.clone());
        self.completion.changed.notify(usize::MAX);
        accepted
    }

    pub fn reject(self, error: CommandError) {
        self.completion.finish(Err(error));
    }
}

impl Drop for CommandRequest {
    fn drop(&mut self) {
        self.completion.finish(Err(CommandError::Cancelled));
    }
}

#[cfg(test)]
mod tests;
