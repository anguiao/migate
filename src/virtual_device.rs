use std::sync::Mutex;

use event_listener::Event;

use crate::device::{Command, Snapshot, VIRTUAL_LIGHT_ID};

#[derive(Default)]
struct State {
    power: bool,
    revision: u64,
}

#[derive(Default)]
pub struct VirtualLight {
    state: Mutex<State>,
    changed: Event,
}

impl VirtualLight {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Snapshot {
        snapshot(&self.state.lock().unwrap())
    }

    pub fn execute(&self, command: Command) -> Snapshot {
        self.execute_with_revision(command).0
    }

    pub(crate) fn execute_with_revision(&self, command: Command) -> (Snapshot, u64) {
        let mut state = self.state.lock().unwrap();
        let power = match command {
            Command::On => true,
            Command::Off => false,
            Command::Toggle => !state.power,
        };
        if power != state.power {
            state.power = power;
            state.revision = state.revision.wrapping_add(1);
            self.changed.notify(usize::MAX);
        }
        (snapshot(&state), state.revision)
    }

    pub fn subscribe(&self) -> Changes<'_> {
        Changes {
            light: self,
            revision: self.state.lock().unwrap().revision,
        }
    }
}

fn snapshot(state: &State) -> Snapshot {
    Snapshot {
        id: VIRTUAL_LIGHT_ID,
        power: state.power,
    }
}

pub struct Changes<'a> {
    light: &'a VirtualLight,
    revision: u64,
}

impl Changes<'_> {
    /// Wait for a real change since subscribing or the previous returned snapshot.
    /// Consecutive changes may be coalesced; cancellation does not consume a change.
    pub async fn changed(&mut self) -> Snapshot {
        self.changed_with_revision().await.0
    }

    pub(crate) async fn changed_with_revision(&mut self) -> (Snapshot, u64) {
        loop {
            // Register first so a command between the check and await cannot be lost.
            let listener = self.light.changed.listen();
            {
                let state = self.light.state.lock().unwrap();
                if state.revision != self.revision {
                    self.revision = state.revision;
                    return (snapshot(&state), state.revision);
                }
            }
            listener.await;
        }
    }
}
