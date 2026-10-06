use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use event_listener::Event;

use super::{
    model::{Capability, Command, CommandError, FunctionSnapshot},
    state::Function,
};

#[derive(Default)]
pub(super) struct Queue {
    pending: VecDeque<PendingCommand>,
    active: Option<Arc<Completion>>,
    pub(super) closed: bool,
}

impl Queue {
    pub(super) fn drain_pending(&mut self) -> Vec<Arc<Completion>> {
        self.pending
            .drain(..)
            .map(|pending| pending.completion)
            .collect()
    }
    pub(super) fn drain_completions(&mut self) -> Vec<Arc<Completion>> {
        let active = self.active.take();
        self.drain_pending().into_iter().chain(active).collect()
    }
}

impl Function {
    /// Completion means the downstream adapter accepted the command, not that
    /// the physical action has been independently confirmed.
    /// Dropping the future withdraws a queued command. An adapter that already
    /// received it still owns its result and may update the shared snapshot.
    pub async fn execute(&self, command: Command) -> Result<FunctionSnapshot, CommandError> {
        if !self.description().capabilities.contains(&Capability::OnOff) {
            return Err(CommandError::Unsupported);
        }
        let completion = Arc::new(Completion::default());
        {
            let mut state = self.0.state.lock().unwrap();
            if state.queue.closed {
                return Err(CommandError::Cancelled);
            }
            self.snapshot_locked(&state).availability.command_result()?;
            state.queue.pending.push_back(PendingCommand {
                command,
                completion: completion.clone(),
            });
        }
        let _submission = Submission {
            function: self.clone(),
            completion: completion.clone(),
        };
        self.0.submitted.notify(usize::MAX);
        completion.wait().await
    }
}

#[derive(Default)]
pub(super) struct Completion {
    result: Mutex<Option<Result<FunctionSnapshot, CommandError>>>,
    changed: Event,
}

impl Completion {
    pub(super) fn finish(&self, result: Result<FunctionSnapshot, CommandError>) {
        let mut current = self.result.lock().unwrap();
        if current.is_none() {
            *current = Some(result);
            self.changed.notify(usize::MAX);
        }
    }

    async fn wait(&self) -> Result<FunctionSnapshot, CommandError> {
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
    completion: Arc<Completion>,
}

/// A waiting caller owns its place in the queue, until the adapter takes over.
struct Submission {
    function: Function,
    completion: Arc<Completion>,
}

impl Drop for Submission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        state
            .queue
            .pending
            .retain(|pending| !Arc::ptr_eq(&pending.completion, &self.completion));
    }
}

/// The single command consumer preserves receive order across adapter sessions.
pub struct CommandReceiver {
    pub(super) function: Function,
}

impl CommandReceiver {
    pub async fn next(&mut self) -> Option<CommandRequest> {
        let active = self.function.0.state.lock().unwrap().queue.active.clone();
        if let Some(active) = active {
            let _ = active.wait().await;
            self.function.0.state.lock().unwrap().queue.active = None;
        }
        loop {
            let listener = self.function.0.submitted.listen();
            let request = {
                let mut state = self.function.0.state.lock().unwrap();
                if let Some(pending) = state.queue.pending.pop_front() {
                    let snapshot = self.function.snapshot_locked(&state);
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
                        function: self.function.clone(),
                        completion: pending.completion,
                    })
                } else if state.queue.closed {
                    return None;
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
        self.function.close();
    }
}

/// A protocol-independent absolute write owned by the adapter until completion.
pub struct CommandRequest {
    pub target: bool,
    function: Function,
    completion: Arc<Completion>,
}

impl CommandRequest {
    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        self.complete(true)
    }

    /// Complete an accepted command without overwriting an observation that the
    /// adapter has already chosen to retain.
    pub fn accept_preserving_state(self) -> Result<FunctionSnapshot, CommandError> {
        self.complete(false)
    }

    fn complete(self, apply_target: bool) -> Result<FunctionSnapshot, CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        let mut result = self.completion.result.lock().unwrap();
        if let Some(completed) = &*result {
            return completed.clone();
        }
        let accepted = Ok(if apply_target {
            self.function.update_power(&mut state, self.target)
        } else {
            self.function.snapshot_locked(&state)
        });
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
