use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use event_listener::Event;

use super::{
    model::{Capability, Command, CommandError, FunctionSnapshot},
    state::{Function, RuntimeState},
};

#[derive(Default)]
pub(super) struct Queue {
    pub(super) pending: VecDeque<PendingCommand>,
    pub(super) active: Option<Arc<Completion>>,
    active_power: Option<Arc<PowerFlight>>,
    pub(super) closed: bool,
}

impl Queue {
    pub(super) fn invalidate_light_on(&mut self) {
        self.pending.retain(|pending| {
            if pending.command == Command::On {
                pending.completion.finish(Err(CommandError::Superseded));
                false
            } else {
                true
            }
        });
        if self
            .active_power
            .as_ref()
            .is_some_and(|flight| flight.target)
        {
            self.invalidate_light_power(CommandError::Superseded);
        }
    }
    pub(super) fn invalidate_light_power(&mut self, error: CommandError) {
        if let Some(flight) = &self.active_power
            && !flight.sent.load(Ordering::Relaxed)
        {
            *flight.invalid.lock().unwrap() = Some(error.clone());
            flight.completion.finish(Err(error));
        }
    }
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
            let (command, order) = if state.light_commands.is_some() {
                let target = match command {
                    Command::On => true,
                    Command::Off => false,
                    Command::Toggle => state
                        .power
                        .value()
                        .map(|value| !value)
                        .ok_or(CommandError::UnknownState)?,
                };
                let coordinator = state.light_commands.as_mut().unwrap();
                if !target {
                    coordinator.cancel_all(CommandError::Superseded);
                    self.notify_light_actions();
                }
                let order = coordinator.next_order();
                if !target {
                    state.queue.invalidate_light_on();
                }
                (if target { Command::On } else { Command::Off }, order)
            } else {
                (command, 0)
            };
            state.queue.pending.push_back(PendingCommand {
                command,
                completion: completion.clone(),
                order,
                queued_at: Instant::now(),
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

    pub(super) fn result(&self) -> Option<Result<FunctionSnapshot, CommandError>> {
        self.result.lock().unwrap().clone()
    }
    pub(super) fn is_finished(&self) -> bool {
        self.result.lock().unwrap().is_some()
    }
    pub(super) async fn wait(&self) -> Result<FunctionSnapshot, CommandError> {
        loop {
            let listener = self.changed.listen();
            if let Some(result) = self.result.lock().unwrap().clone() {
                return result;
            }
            listener.await;
        }
    }
}

pub(super) struct PendingCommand {
    command: Command,
    completion: Arc<Completion>,
    pub(super) order: u64,
    queued_at: Instant,
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
    pub(super) async fn wait_active(&self) {
        let active = self.function.0.state.lock().unwrap().queue.active.clone();
        if let Some(active) = active {
            let _ = active.wait().await;
            let mut state = self.function.0.state.lock().unwrap();
            if state
                .queue
                .active
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &active))
            {
                state.queue.active = None;
                state.queue.active_power = None;
            }
        }
    }
    pub(super) fn take_power(&self, state: &mut RuntimeState) -> Option<CommandRequest> {
        while let Some(pending) = state.queue.pending.pop_front() {
            let snapshot = self.function.snapshot_locked(state);
            let target =
                snapshot
                    .availability
                    .command_result()
                    .and_then(|()| match pending.command {
                        Command::On => Ok(true),
                        Command::Off => Ok(false),
                        Command::Toggle => snapshot
                            .power
                            .value()
                            .map(|value| !value)
                            .ok_or(CommandError::UnknownState),
                    });
            let target = match target {
                Ok(target) => target,
                Err(error) => {
                    pending.completion.finish(Err(error));
                    continue;
                }
            };
            let flight = state.light_commands.as_ref().map(|_| {
                Arc::new(PowerFlight {
                    target,
                    completion: pending.completion.clone(),
                    sent: AtomicBool::new(false),
                    invalid: Mutex::new(None),
                })
            });
            state.queue.active = Some(pending.completion.clone());
            state.queue.active_power = flight.clone();
            return Some(CommandRequest {
                target,
                queued_at: pending.queued_at,
                function: self.function.clone(),
                completion: pending.completion,
                flight,
            });
        }
        None
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
    queued_at: Instant,
    function: Function,
    completion: Arc<Completion>,
    flight: Option<Arc<PowerFlight>>,
}
struct PowerFlight {
    target: bool,
    completion: Arc<Completion>,
    sent: AtomicBool,
    invalid: Mutex<Option<CommandError>>,
}

impl CommandRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.queued_at
    }

    /// Lighting power writes use the same actual-send guard as numeric frames.
    /// Outlet request ownership continues to transfer on dequeue.
    pub fn is_current(&self) -> bool {
        self.current_result().is_ok()
    }
    /// Inspect a lighting power cancellation without marking it sent.
    pub fn current_result(&self) -> Result<(), CommandError> {
        let Some(flight) = &self.flight else {
            return Ok(());
        };
        if let Some(error) = flight.invalid.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if self.completion.is_finished() {
            return Err(self
                .completion
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        Ok(())
    }
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        if let Some(flight) = &self.flight {
            if let Some(error) = flight.invalid.lock().unwrap().as_ref() {
                return Err(error.clone());
            }
            flight.sent.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
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
        if apply_target {
            self.function.update_power(&mut state, self.target);
        }
        let accepted = Ok(self.function.snapshot_locked(&state));
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
