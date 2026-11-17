use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use super::{CommandError, Function, FunctionSnapshot, command::Completion, state::RuntimeState};

/// Defines which queued operations a new operation can replace or interrupt.
pub(super) trait StepCommand: Copy {
    const MISSING_WRITE: &'static str;

    fn coordinator(state: &RuntimeState) -> Option<&StepCoordinator<Self>>;
    fn coordinator_mut(state: &mut RuntimeState) -> Option<&mut StepCoordinator<Self>>;
    fn supersedes(self, previous: Self) -> bool;
    fn interrupts(self, previous: Self) -> bool;
    fn follow(self, _previous: &Arc<StepEntry<Self>>) -> Option<Arc<StepEntry<Self>>> {
        None
    }
}

pub(super) struct StepEntry<C> {
    pub(super) command: C,
    completion: Completion,
    gate: Completion,
    started: AtomicBool,
    in_flight: AtomicBool,
    interrupted: Mutex<Option<CommandError>>,
    queued_at: Instant,
    pub(super) preceding: Option<Arc<StepEntry<C>>>,
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

    pub(super) fn unsent_failure(&self) -> Option<CommandError> {
        self.completion
            .result()
            .and_then(Result::err)
            .filter(|_| !self.started())
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

    pub(super) fn accept_unchanged(
        &self,
        snapshot: impl FnOnce() -> FunctionSnapshot,
    ) -> Result<FunctionSnapshot, CommandError> {
        self.current_result()?;
        assert!(!self.started());
        let snapshot = snapshot();
        self.finish(Ok(snapshot.clone()));
        Ok(snapshot)
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
    closed: bool,
}
impl<C> Default for StepCoordinator<C> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            flight: None,
            closed: false,
        }
    }
}
impl<C: StepCommand> StepCoordinator<C> {
    pub(super) fn submit(&mut self, command: C) -> Arc<StepEntry<C>> {
        let preceding = self
            .pending
            .back()
            .or(self.flight.as_ref())
            .filter(|previous| !previous.completion.is_finished())
            .and_then(|previous| command.follow(previous));
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
            preceding,
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

/// The acceptance of one queued operation, independent of the device's steps.
pub(super) struct StepAction<C>(pub(super) Arc<StepEntry<C>>);
impl<C> StepAction<C> {
    pub(super) async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

/// Resolve a device command at the common admission boundary.
pub(super) fn submit<C: StepCommand>(
    function: &Function,
    resolve: impl FnOnce(&RuntimeState) -> Result<C, CommandError>,
) -> Result<StepAction<C>, CommandError> {
    let mut state = function.0.state.lock().unwrap();
    let commands = C::coordinator(&state).ok_or(CommandError::Unsupported)?;
    if commands.is_closed() {
        return Err(CommandError::Cancelled);
    }
    function
        .snapshot_locked(&state)
        .availability
        .command_result()?;
    let command = resolve(&state)?;
    let action = StepAction(C::coordinator_mut(&mut state).unwrap().submit(command));
    function.0.submitted.notify(usize::MAX);
    Ok(action)
}

/// Wait for the original sent receipt before taking the next operation.
pub(super) async fn next<C: StepCommand>(function: &Function) -> Option<StepRequest<C>> {
    let flight = C::coordinator(&function.0.state.lock().unwrap())
        .unwrap()
        .flight();
    if let Some(flight) = flight {
        flight.wait_gate().await;
        C::coordinator_mut(&mut function.0.state.lock().unwrap())
            .unwrap()
            .release_flight(&flight);
    }
    loop {
        let listener = function.0.submitted.listen();
        {
            let mut state = function.0.state.lock().unwrap();
            let commands = C::coordinator_mut(&mut state).unwrap();
            if let Some(entry) = commands.take_next() {
                return Some(StepRequest {
                    function: function.clone(),
                    entry,
                });
            }
            if commands.is_closed() {
                return None;
            }
        }
        listener.await;
    }
}

/// Owns the common receipt and cancellation of one operation.
pub(super) struct StepRequest<C: StepCommand> {
    pub(super) function: Function,
    pub(super) entry: Arc<StepEntry<C>>,
}
impl<C: StepCommand> StepRequest<C> {
    pub(super) fn queued_at(&self) -> Instant {
        self.entry.queued_at()
    }
    pub(super) fn current_result(&self) -> Result<(), CommandError> {
        self.entry.current_result()
    }
    // The caller holds the function state lock for the complete device step.
    pub(super) fn mark_sent(&self) -> Result<(), CommandError> {
        self.entry.mark_sent()
    }
    pub(super) fn step_accepted(&self) -> Result<(), CommandError> {
        self.entry.step_accepted()
    }
    pub(super) fn accept(&self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        self.entry
            .accept(|| self.function.snapshot_locked(&state), C::MISSING_WRITE)
    }
    pub(super) fn accept_unchanged(&self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        self.entry
            .accept_unchanged(|| self.function.snapshot_locked(&state))
    }
    pub(super) fn reject(&self, error: CommandError) {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.reject(error);
    }
}
impl<C: StepCommand> Drop for StepRequest<C> {
    fn drop(&mut self) {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.cancel();
    }
}

/// A dropped power caller withdraws an operation only while it is still queued.
pub(super) struct StepSubmission<C: StepCommand> {
    function: Function,
    entry: Arc<StepEntry<C>>,
}
impl<C: StepCommand> StepSubmission<C> {
    pub(super) fn new(function: Function, entry: Arc<StepEntry<C>>) -> Self {
        Self { function, entry }
    }
}
impl<C: StepCommand> Drop for StepSubmission<C> {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        C::coordinator_mut(&mut state)
            .unwrap()
            .cancel_pending(&self.entry);
    }
}
