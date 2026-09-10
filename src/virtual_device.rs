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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::{block_on, poll_once};
    use std::sync::Arc;

    #[test]
    fn commands_share_one_state_and_toggles_are_serial() {
        let light = Arc::new(VirtualLight::new());
        assert_eq!(light.snapshot().id, "virtual-light-1");
        assert!(!light.snapshot().power);
        let workers: Vec<_> = (0..5)
            .map(|_| {
                let light = light.clone();
                std::thread::spawn(move || {
                    for _ in 0..101 {
                        light.execute(Command::Toggle);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(light.snapshot().power);
        assert!(!light.execute(Command::Off).power);
        assert!(light.execute(Command::On).power);
    }

    #[test]
    fn notifications_track_real_changes_and_latest_snapshot() {
        block_on(async {
            let light = VirtualLight::new();
            let mut changes = light.subscribe();
            light.execute(Command::Off);
            assert!(poll_once(changes.changed()).await.is_none());
            light.execute(Command::On);
            light.execute(Command::Off);
            light.execute(Command::On);
            assert!(changes.changed().await.power);
            assert!(poll_once(changes.changed()).await.is_none());
            light.execute(Command::On);
            assert!(poll_once(changes.changed()).await.is_none());
            light.execute(Command::Off);
            assert!(!changes.changed().await.power);
        });
    }
}
