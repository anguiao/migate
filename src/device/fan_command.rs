use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use super::{
    Command, CommandError, CommandReceiver, DeviceRequest, Function, FunctionKind,
    FunctionSnapshot, command::Completion,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanCommand {
    Power(bool),
    Speed(u8),
    Swing(bool),
}
impl FanCommand {
    fn is_speed(self) -> bool {
        matches!(self, Self::Speed(1..=100))
    }
    fn is_off(self) -> bool {
        matches!(self, Self::Power(false) | Self::Speed(0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FanStep {
    Power(bool),
    Auxiliary,
    Speed(u8),
    Swing(bool),
}

struct Entry {
    command: FanCommand,
    completion: Completion,
    gate: Completion,
    started: AtomicBool,
    in_flight: AtomicBool,
    interrupted: Mutex<Option<CommandError>>,
    queued_at: Instant,
}
impl Entry {
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
}

pub struct FanAction(Arc<Entry>);
impl FanAction {
    /// A successful result means all required writes were accepted.
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.completion.wait().await
    }
}

#[derive(Default)]
pub(super) struct FanCoordinator {
    pending: VecDeque<Arc<Entry>>,
    flight: Option<Arc<Entry>>,
    confirmed_power_on: bool,
    closed: bool,
}
impl FanCoordinator {
    fn submit(&mut self, command: FanCommand) -> FanAction {
        if command.is_off() {
            self.pending.retain(|entry| {
                if entry.command.is_speed() {
                    entry.invalidate(CommandError::Superseded, true);
                    false
                } else {
                    true
                }
            });
            if let Some(entry) = &self.flight
                && entry.command.is_speed()
            {
                entry.invalidate(CommandError::Superseded, true);
            }
        } else if command.is_speed() {
            if let Some(entry) = self.pending.pop_back_if(|entry| entry.command.is_speed()) {
                entry.invalidate(CommandError::Superseded, true);
            } else if self.pending.is_empty()
                && let Some(entry) = &self.flight
                && entry.command.is_speed()
                && !entry.started.load(Ordering::Relaxed)
            {
                entry.invalidate(CommandError::Superseded, true);
            }
        }
        let entry = Arc::new(Entry {
            command,
            completion: Completion::default(),
            gate: Completion::default(),
            started: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            interrupted: Mutex::new(None),
            queued_at: Instant::now(),
        });
        self.pending.push_back(entry.clone());
        FanAction(entry)
    }
    pub(super) fn observe_power(&mut self, power: bool, fresh: bool) {
        if fresh || !power {
            self.confirmed_power_on = power;
        }
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

impl Function {
    pub(crate) fn pending_fan_commands(&self) -> Vec<FanCommand> {
        let state = self.0.state.lock().unwrap();
        let Some(commands) = state.fan_commands() else {
            return Vec::new();
        };
        commands
            .flight
            .iter()
            .filter(|entry| {
                !entry.started.load(Ordering::Relaxed) && !entry.completion.is_finished()
            })
            .chain(commands.pending.iter())
            .map(|entry| entry.command)
            .collect()
    }
    fn submit_fan_locked(
        &self,
        state: &mut super::state::RuntimeState,
        command: FanCommand,
    ) -> Result<FanAction, CommandError> {
        let commands = state.fan_commands().ok_or(CommandError::Unsupported)?;
        if commands.closed {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(state).availability.command_result()?;
        let action = state.fan_commands_mut().unwrap().submit(command);
        self.0.submitted.notify(usize::MAX);
        Ok(action)
    }

    /// Queue a fan operation without changing the observed device state.
    pub fn begin_fan(&self, command: FanCommand) -> Result<super::FanAction, CommandError> {
        if let (FanCommand::Speed(speed), FunctionKind::Fan { speed_count, .. }) =
            (command, self.description().kind)
            && speed > speed_count
        {
            return Err(CommandError::Rejected(
                "fan speed is outside the available levels".into(),
            ));
        }
        if matches!(command, FanCommand::Swing(_))
            && !matches!(
                self.description().kind,
                FunctionKind::Fan {
                    horizontal_swing: true,
                    ..
                }
            )
        {
            return Err(CommandError::Unsupported);
        }
        let mut state = self.0.state.lock().unwrap();
        self.submit_fan_locked(&mut state, command)
    }

    pub(super) async fn execute_fan_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = self.begin_fan_power(command)?;
        let _submission = PowerSubmission {
            function: self.clone(),
            entry: action.0.clone(),
        };
        action.accepted().await
    }
    fn begin_fan_power(&self, command: Command) -> Result<FanAction, CommandError> {
        if command == Command::On {
            return self.begin_fan(FanCommand::Power(true));
        }
        if command == Command::Off {
            return self.begin_fan(FanCommand::Power(false));
        }
        let mut state = self.0.state.lock().unwrap();
        let commands = state.fan_commands().ok_or(CommandError::Unsupported)?;
        if commands.closed {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(&state).availability.command_result()?;
        let target = match command {
            Command::On | Command::Off => unreachable!(),
            Command::Toggle => !state.fan_power().ok_or(CommandError::UnknownState)?,
        };
        self.submit_fan_locked(&mut state, FanCommand::Power(target))
    }
}

struct PowerSubmission {
    function: Function,
    entry: Arc<Entry>,
}
impl Drop for PowerSubmission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        let commands = state.fan_commands_mut().unwrap();
        if let Some(index) = commands
            .pending
            .iter()
            .position(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            commands.pending.remove(index);
            self.entry.invalidate(CommandError::Cancelled, true);
        }
    }
}

impl CommandReceiver {
    pub(super) async fn next_fan_operation(&mut self) -> Option<DeviceRequest> {
        let flight = self
            .function
            .0
            .state
            .lock()
            .unwrap()
            .fan_commands()
            .unwrap()
            .flight
            .clone();
        if let Some(flight) = flight {
            let _ = flight.gate.wait().await;
            let mut state = self.function.0.state.lock().unwrap();
            let commands = state.fan_commands_mut().unwrap();
            if commands
                .flight
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                commands.flight = None;
            }
        }
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let commands = state.fan_commands_mut().unwrap();
                if let Some(entry) = commands.pending.pop_front() {
                    commands.flight = Some(entry.clone());
                    return Some(DeviceRequest::Fan(FanRequest {
                        command: entry.command,
                        function: self.function.clone(),
                        entry,
                    }));
                }
                if commands.closed {
                    return None;
                }
            }
            listener.await;
        }
    }
}

/// Owns every step of one operation until the original receipt settles.
pub struct FanRequest {
    pub command: FanCommand,
    function: Function,
    entry: Arc<Entry>,
}
impl FanRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.entry.queued_at
    }
    pub(crate) fn needs_power_on(&self) -> bool {
        let state = self.function.0.state.lock().unwrap();
        !state.fan_commands().unwrap().confirmed_power_on
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        match self.entry.completion.result() {
            Some(Err(error)) => Err(error),
            Some(Ok(_)) => Err(CommandError::Cancelled),
            None => self
                .entry
                .interrupted
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err),
        }
    }
    pub(crate) fn mark_sent(&self, step: FanStep) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        self.current_result()?;
        assert!(!self.entry.in_flight.load(Ordering::Relaxed));
        self.entry.started.store(true, Ordering::Relaxed);
        self.entry.in_flight.store(true, Ordering::Relaxed);
        if step == FanStep::Power(false) {
            state.fan_commands_mut().unwrap().confirmed_power_on = false;
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        assert!(self.entry.in_flight.swap(false, Ordering::Relaxed));
        let result = self.current_result();
        if let Err(error) = &result {
            self.entry.completion.finish(Err(error.clone()));
            self.entry.gate.finish(Err(error.clone()));
        }
        result
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        let result = if let Some(result) = self.entry.completion.result() {
            result
        } else if !self.entry.started.load(Ordering::Relaxed)
            || self.entry.in_flight.load(Ordering::Relaxed)
        {
            Err(CommandError::SendFailed(
                "fan request has no settled write".into(),
            ))
        } else {
            Ok(self.function.snapshot_locked(&state))
        };
        self.entry.completion.finish(result.clone());
        self.entry.gate.finish(result.clone());
        result
    }
    pub(crate) fn reject(self, error: CommandError) {
        let _state = self.function.0.state.lock().unwrap();
        let result = self.entry.completion.result().unwrap_or(Err(error));
        self.entry.completion.finish(result.clone());
        self.entry.gate.finish(result);
    }
}
impl Drop for FanRequest {
    fn drop(&mut self) {
        let _state = self.function.0.state.lock().unwrap();
        let result = self.entry.completion.result().unwrap_or_else(|| {
            Err(self
                .entry
                .interrupted
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(CommandError::Cancelled))
        });
        self.entry.completion.finish(result.clone());
        self.entry.gate.finish(result);
    }
}
