use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use super::{CommandError, FunctionSnapshot, command::Completion};

/// Defines which queued operations a new operation can replace or interrupt.
pub(super) trait StepCommand: Copy {
    fn supersedes(self, previous: Self) -> bool;
    fn interrupts(self, previous: Self) -> bool;
}

pub(super) struct StepEntry<C> {
    pub(super) command: C,
    completion: Completion,
    gate: Completion,
    started: AtomicBool,
    in_flight: AtomicBool,
    interrupted: Mutex<Option<CommandError>>,
    queued_at: Instant,
}
impl<C> StepEntry<C> {
    fn invalidate(&self, error: CommandError, immediate: bool) {
        let in_flight = self.in_flight.load(Ordering::Relaxed);
        if in_flight && !immediate {
            *self.interrupted.lock().unwrap() = Some(error.clone());
        } else {
            self.completion.finish(Err(error.clone()));
        }
        if !in_flight {
            self.gate.finish(Err(error));
        }
    }

    pub(super) async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.completion.wait().await
    }

    pub(super) async fn wait_gate(&self) {
        let _ = self.gate.wait().await;
    }

    pub(super) fn queued_at(&self) -> Instant {
        self.queued_at
    }

    pub(super) fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    pub(super) fn current_result(&self) -> Result<(), CommandError> {
        match self.completion.result() {
            Some(Err(error)) => Err(error),
            Some(Ok(_)) => Err(CommandError::Cancelled),
            None => self.interrupted.lock().unwrap().clone().map_or(Ok(()), Err),
        }
    }

    // The caller holds the function state lock when mutating an entry.
    pub(super) fn mark_sent(&self) -> Result<(), CommandError> {
        self.current_result()?;
        assert!(!self.in_flight.load(Ordering::Relaxed));
        self.started.store(true, Ordering::Relaxed);
        self.in_flight.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn step_accepted(&self) -> Result<(), CommandError> {
        assert!(self.in_flight.swap(false, Ordering::Relaxed));
        let result = self.current_result();
        if let Err(error) = &result {
            self.completion.finish(Err(error.clone()));
            self.gate.finish(Err(error.clone()));
        }
        result
    }

    pub(super) fn accept(
        &self,
        snapshot: impl FnOnce() -> FunctionSnapshot,
        missing_write: &str,
    ) -> Result<FunctionSnapshot, CommandError> {
        let result = if let Some(result) = self.completion.result() {
            result
        } else if !self.started.load(Ordering::Relaxed) || self.in_flight.load(Ordering::Relaxed) {
            Err(CommandError::SendFailed(missing_write.into()))
        } else {
            Ok(snapshot())
        };
        self.finish(result.clone());
        result
    }

    pub(super) fn reject(&self, error: CommandError) {
        let result = self.completion.result().unwrap_or(Err(error));
        self.finish(result);
    }

    pub(super) fn cancel(&self) {
        let result = self.completion.result().unwrap_or_else(|| {
            Err(self
                .interrupted
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(CommandError::Cancelled))
        });
        self.finish(result);
    }

    fn finish(&self, result: Result<FunctionSnapshot, CommandError>) {
        self.completion.finish(result.clone());
        self.gate.finish(result);
    }
}

/// Keeps one logical operation in flight until its original sent step settles.
pub(super) struct StepCoordinator<C> {
    pending: VecDeque<Arc<StepEntry<C>>>,
    flight: Option<Arc<StepEntry<C>>>,
    confirmed_power_on: bool,
    closed: bool,
}
impl<C> Default for StepCoordinator<C> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            flight: None,
            confirmed_power_on: false,
            closed: false,
        }
    }
}
impl<C: StepCommand> StepCoordinator<C> {
    pub(super) fn submit(&mut self, command: C) -> Arc<StepEntry<C>> {
        self.pending.retain(|entry| {
            if command.interrupts(entry.command) {
                entry.invalidate(CommandError::Superseded, true);
                false
            } else {
                true
            }
        });
        if let Some(entry) = &self.flight
            && command.interrupts(entry.command)
        {
            entry.invalidate(CommandError::Superseded, true);
        }
        if let Some(entry) = self
            .pending
            .pop_back_if(|entry| command.supersedes(entry.command))
        {
            entry.invalidate(CommandError::Superseded, true);
        } else if self.pending.is_empty()
            && let Some(entry) = &self.flight
            && command.supersedes(entry.command)
            && !entry.started.load(Ordering::Relaxed)
        {
            entry.invalidate(CommandError::Superseded, true);
        }
        let entry = Arc::new(StepEntry {
            command,
            completion: Completion::default(),
            gate: Completion::default(),
            started: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            interrupted: Mutex::new(None),
            queued_at: Instant::now(),
        });
        self.pending.push_back(entry.clone());
        entry
    }

    pub(super) fn pending_commands(&self) -> Vec<C> {
        self.flight
            .iter()
            .filter(|entry| {
                !entry.started.load(Ordering::Relaxed) && !entry.completion.is_finished()
            })
            .chain(self.pending.iter())
            .map(|entry| entry.command)
            .collect()
    }

    pub(super) fn flight(&self) -> Option<Arc<StepEntry<C>>> {
        self.flight.clone()
    }

    pub(super) fn release_flight(&mut self, flight: &Arc<StepEntry<C>>) {
        if self
            .flight
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            self.flight = None;
        }
    }

    pub(super) fn take_next(&mut self) -> Option<Arc<StepEntry<C>>> {
        let entry = self.pending.pop_front()?;
        self.flight = Some(entry.clone());
        Some(entry)
    }

    pub(super) fn cancel_pending(&mut self, entry: &Arc<StepEntry<C>>) {
        if let Some(index) = self
            .pending
            .iter()
            .position(|pending| Arc::ptr_eq(pending, entry))
        {
            self.pending.remove(index);
            entry.invalidate(CommandError::Cancelled, true);
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(super) fn observe_power(&mut self, power: bool, fresh: bool) {
        if fresh || !power {
            self.confirmed_power_on = power;
        }
    }

    pub(super) fn needs_power_on(&self) -> bool {
        !self.confirmed_power_on
    }

    pub(super) fn power_off_sent(&mut self) {
        self.confirmed_power_on = false;
    }

    pub(super) fn unavailable(&mut self, error: CommandError) {
        for entry in self.pending.drain(..) {
            entry.invalidate(error.clone(), false);
        }
        if let Some(entry) = &self.flight {
            entry.invalidate(error, false);
        }
    }

    pub(super) fn close(&mut self, error: CommandError) {
        self.closed = true;
        self.unavailable(error);
    }
}
