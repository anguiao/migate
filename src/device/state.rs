use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use event_listener::Event;

use super::{
    command::{CommandReceiver, Queue},
    model::{Availability, DeviceDescription, DeviceSnapshot, Power},
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
    pub(super) snapshot: DeviceSnapshot,
    pub(super) queue: Queue,
}

pub(super) struct Inner {
    description: DeviceDescription,
    pub(super) state: Mutex<RuntimeState>,
    changed: Event,
    pub(super) submitted: Event,
    pub(super) clock: Box<dyn Clock>,
}

/// One shared state and command boundary for every protocol adapter.
#[derive(Clone)]
pub struct Device(pub(super) Arc<Inner>);

impl Device {
    pub fn new(description: DeviceDescription) -> (Self, CommandReceiver) {
        Self::with_clock(description, Box::new(SystemClock(Instant::now())))
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        description: DeviceDescription,
        clock: Box<dyn Clock>,
    ) -> (Self, CommandReceiver) {
        Self::with_clock(description, clock)
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

    pub(super) fn update_snapshot(
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
