use super::*;

impl Function {
    /// Completion means the downstream adapter accepted the command, not that
    /// the physical action has been independently confirmed.
    /// Dropping the future withdraws a queued command. An adapter that already
    /// received it still owns its result and may update the shared snapshot.
    pub async fn execute(&self, command: Command) -> Result<FunctionSnapshot, CommandError> {
        let completion = Arc::new(Completion::default());
        let light = self.description().kind.is_light();
        let resolved = {
            let mut state = self.0.state.lock().unwrap();
            let Some(commands) = state.commands() else {
                return Err(CommandError::Unsupported);
            };
            if commands.is_closed() {
                return Err(CommandError::Cancelled);
            }
            self.snapshot_locked(&state).availability.command_result()?;
            let resolved = if light {
                match command {
                    Command::On => Command::On,
                    Command::Off => Command::Off,
                    Command::Toggle => {
                        let value = state
                            .power()
                            .and_then(super::super::Power::value)
                            .ok_or(CommandError::UnknownState)?;
                        if value { Command::Off } else { Command::On }
                    }
                }
            } else {
                command
            };
            state
                .commands_mut()
                .expect("checked command queue exists")
                .submit_power(resolved, light, completion.clone());
            resolved
        };
        if light && resolved == Command::Off {
            self.notify_light_actions();
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

/// A waiting caller owns its place in the queue, until the adapter takes over.
struct Submission {
    function: Function,
    completion: Arc<Completion>,
}

impl Drop for Submission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        if let Some(commands) = state.commands_mut() {
            commands
                .power
                .retain(|pending| !Arc::ptr_eq(&pending.completion, &self.completion));
        }
    }
}

/// The single command consumer preserves receive order across adapter sessions.
pub struct CommandReceiver {
    pub(in crate::device) function: Function,
}

impl CommandReceiver {
    async fn wait_active(&self) {
        let flight = self
            .function
            .0
            .state
            .lock()
            .unwrap()
            .commands()
            .expect("read-only functions have no command receiver")
            .flight
            .clone();
        if let Some(flight) = flight {
            let _ = flight.gate().wait().await;
            let mut state = self.function.0.state.lock().unwrap();
            let commands = state
                .commands_mut()
                .expect("read-only functions have no command receiver");
            if commands
                .flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                commands.flight = None;
            }
        }
    }
    /// One function consumes power in FIFO order and resolves Toggle at dequeue.
    fn take_power(&self, state: &mut RuntimeState) -> Option<CommandRequest> {
        loop {
            let pending = state
                .commands_mut()
                .expect("read-only functions have no command receiver")
                .power
                .pop_front()?;
            let snapshot = self.function.snapshot_locked(state);
            let target =
                snapshot
                    .availability
                    .command_result()
                    .and_then(|()| match pending.command {
                        Command::On => Ok(true),
                        Command::Off => Ok(false),
                        Command::Toggle => snapshot
                            .power()
                            .and_then(super::super::Power::value)
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
            let flight = Arc::new(PowerFlight {
                target,
                completion: pending.completion.clone(),
                sent: AtomicBool::new(false),
                invalid: Mutex::new(None),
            });
            state
                .commands_mut()
                .expect("read-only functions have no command receiver")
                .flight = Some(Arc::new(Flight::Power(flight.clone())));
            return Some(CommandRequest {
                target,
                queued_at: pending.queued_at,
                function: self.function.clone(),
                flight,
            });
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
    queued_at: Instant,
    function: Function,
    flight: Arc<PowerFlight>,
}

impl CommandRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.queued_at
    }

    /// Inspect a lighting power cancellation without marking it sent.
    pub fn current_result(&self) -> Result<(), CommandError> {
        if let Some(error) = self.flight.invalid.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if self.flight.completion.is_finished() {
            return Err(self
                .flight
                .completion
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        Ok(())
    }
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        if let Some(error) = self.flight.invalid.lock().unwrap().as_ref() {
            return Err(error.clone());
        }
        if self.flight.completion.is_finished() {
            return Err(self
                .flight
                .completion
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Cancelled));
        }
        self.flight.sent.store(true, Ordering::Relaxed);
        Ok(())
    }
    /// Settle this request against the current snapshot. The coordinator never
    /// infers physical state from acceptance; source observations own that.
    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        if let Some(completed) = self.flight.completion.result() {
            return completed;
        }
        let accepted = Ok(self.function.snapshot_locked(&state));
        self.flight.completion.finish(accepted.clone());
        accepted
    }

    pub fn reject(self, error: CommandError) {
        self.flight.completion.finish(Err(error));
    }
}

impl Drop for CommandRequest {
    fn drop(&mut self) {
        self.flight.completion.finish(Err(CommandError::Cancelled));
    }
}

pub enum DeviceRequest {
    Power(CommandRequest),
    Light(LightRequest),
}
impl DeviceRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        match self {
            Self::Power(request) => request.queued_at(),
            Self::Light(request) => request.queued_at,
        }
    }
}
impl CommandReceiver {
    /// One gate serializes power and both lighting dimensions for this function.
    pub async fn next_operation(&mut self) -> Option<DeviceRequest> {
        self.wait_active().await;
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let (light, power) = {
                    let commands = state
                        .commands()
                        .expect("read-only functions have no command receiver");
                    (
                        commands.first_light_pending(),
                        commands.power.front().map(|pending| pending.order),
                    )
                };
                if let Some((index, order)) = light
                    && power.is_none_or(|power| order < power)
                {
                    let commands = state
                        .commands_mut()
                        .expect("read-only functions have no command receiver");
                    let frame = commands.light_pending[index].take().unwrap();
                    let token = frame.action.token;
                    let flight = Arc::new(LightFlight {
                        action: frame.action,
                        power: frame.target.power(),
                        gate: Arc::new(Completion::default()),
                        sent: AtomicBool::new(false),
                    });
                    commands.flight = Some(Arc::new(Flight::Light(flight.clone())));
                    return Some(DeviceRequest::Light(LightRequest {
                        token,
                        target: frame.target,
                        final_frame: frame.final_frame,
                        queued_at: frame.queued_at,
                        function: self.function.clone(),
                        flight,
                    }));
                }
                if let Some(request) = self.take_power(&mut state) {
                    return Some(DeviceRequest::Power(request));
                }
                if state
                    .commands()
                    .expect("read-only functions have no command receiver")
                    .is_closed()
                {
                    return None;
                }
            }
            listener.await;
        }
    }
}

/// The adapter owns this frame until its original receipt or timeout completes.
pub struct LightRequest {
    pub token: ActionToken,
    pub target: LightTarget,
    pub final_frame: bool,
    queued_at: Instant,
    function: Function,
    flight: Arc<LightFlight>,
}
impl LightRequest {
    pub fn is_current(&self) -> bool {
        self.current_result().is_ok()
    }
    /// Inspect cancellation without transferring actual-send ownership.
    pub fn current_result(&self) -> Result<(), CommandError> {
        if !self.flight.gate.is_finished() && self.function.light_action_current(self.token) {
            Ok(())
        } else {
            Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded))
        }
    }
    /// Call at the actual transport submission boundary, never just on dequeue.
    pub fn mark_sent(&self) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        let coordinator = state
            .commands_mut()
            .expect("read-only functions have no command receiver");
        if self.flight.sent.load(Ordering::Relaxed) {
            return Ok(());
        }
        if coordinator.light_action(self.token).is_none() || self.flight.gate.is_finished() {
            return Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        self.flight.sent.store(true, Ordering::Relaxed);
        self.flight
            .action
            .startup_sent
            .store(true, Ordering::Relaxed);
        // Diagnostics distinguish an invalidated action from its original receipt.
        coordinator.light_revision += 1;
        self.function.notify_light_actions();
        Ok(())
    }
    pub fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        if !self.flight.sent.load(Ordering::Relaxed) {
            let error = CommandError::SendFailed("lighting frame was not submitted".into());
            self.finish(Err(error.clone()), true);
            return Err(error);
        }
        let snapshot = self.function.snapshot();
        self.finish(Ok(snapshot.clone()), true);
        Ok(snapshot)
    }
    /// Settle a source-confirmed target that needs no transport write.
    /// Transition startup still waits for a write unless its whole target stays constant.
    pub fn accept_unchanged(self) -> Result<FunctionSnapshot, CommandError> {
        if !self.is_current() {
            return Err(self
                .flight
                .action
                .ended
                .result()
                .and_then(Result::err)
                .unwrap_or(CommandError::Superseded));
        }
        if self.flight.sent.load(Ordering::Relaxed) {
            return Err(CommandError::SendFailed(
                "submitted frame requires its transport receipt".into(),
            ));
        }
        let snapshot = self.function.snapshot();
        self.finish(
            Ok(snapshot.clone()),
            self.final_frame || self.flight.action.allow_unchanged_startup,
        );
        Ok(snapshot)
    }
    pub fn reject(self, error: CommandError) {
        self.finish(Err(error), true);
    }
    fn finish(&self, result: Result<FunctionSnapshot, CommandError>, accept_startup: bool) {
        let mut state = self.function.0.state.lock().unwrap();
        if self.flight.gate.is_finished() {
            return;
        }
        let coordinator = state
            .commands_mut()
            .expect("read-only functions have no command receiver");
        if accept_startup {
            if result.is_ok()
                && !self.flight.action.accepted.is_finished()
                && coordinator.light_action(self.token).is_some()
            {
                self.flight
                    .action
                    .accepted_while_current
                    .store(true, Ordering::Relaxed);
            }
            self.flight.action.accepted.finish(result.clone());
        }
        if coordinator.light_action(self.token).is_some() {
            match &result {
                Err(error) => coordinator.cancel_light(self.token.dimension, error.clone()),
                Ok(_) if self.final_frame => {
                    coordinator.light_current[self.token.dimension.index()] = None;
                    coordinator.light_pending[self.token.dimension.index()] = None;
                    self.flight.action.ended.finish(result.clone());
                    coordinator.record_light(self.token, Ok(()));
                }
                Ok(_) if accept_startup => coordinator.record_light(self.token, Ok(())),
                Ok(_) => coordinator.light_revision += 1,
            }
        } else {
            coordinator.light_revision += 1;
        }
        if coordinator.flight.as_ref().is_some_and(
            |flight| matches!(&**flight, Flight::Light(light) if Arc::ptr_eq(light, &self.flight)),
        ) {
            coordinator.flight = None;
        }
        self.flight.gate.finish(result);
        self.function.notify_light_actions();
    }
}
impl Drop for LightRequest {
    fn drop(&mut self) {
        self.finish(Err(CommandError::Cancelled), true);
    }
}
