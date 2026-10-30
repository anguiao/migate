use std::{sync::Arc, time::Instant};

use super::{
    Command, CommandError, CommandReceiver, DeviceRequest, Function, FunctionKind,
    FunctionSnapshot,
    step_command::{StepCommand, StepCoordinator, StepEntry},
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

impl StepCommand for FanCommand {
    fn supersedes(self, previous: Self) -> bool {
        self.is_speed() && previous.is_speed()
    }

    fn interrupts(self, previous: Self) -> bool {
        self.is_off() && previous.is_speed()
    }
}

pub(super) type FanCoordinator = StepCoordinator<FanCommand>;

pub struct FanAction(Arc<StepEntry<FanCommand>>);
impl FanAction {
    /// A successful result means all required writes were accepted.
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

impl Function {
    pub(crate) fn pending_fan_commands(&self) -> Vec<FanCommand> {
        let state = self.0.state.lock().unwrap();
        let Some(commands) = state.fan_commands() else {
            return Vec::new();
        };
        commands.pending_commands()
    }
    fn submit_fan_locked(
        &self,
        state: &mut super::state::RuntimeState,
        command: FanCommand,
    ) -> Result<FanAction, CommandError> {
        let commands = state.fan_commands().ok_or(CommandError::Unsupported)?;
        if commands.is_closed() {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(state).availability.command_result()?;
        let action = FanAction(state.fan_commands_mut().unwrap().submit(command));
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
        if commands.is_closed() {
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
    entry: Arc<StepEntry<FanCommand>>,
}
impl Drop for PowerSubmission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        let commands = state.fan_commands_mut().unwrap();
        commands.cancel_pending(&self.entry);
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
            .flight();
        if let Some(flight) = flight {
            flight.wait_gate().await;
            let mut state = self.function.0.state.lock().unwrap();
            let commands = state.fan_commands_mut().unwrap();
            commands.release_flight(&flight);
        }
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let commands = state.fan_commands_mut().unwrap();
                if let Some(entry) = commands.take_next() {
                    return Some(DeviceRequest::Fan(FanRequest {
                        command: entry.command,
                        function: self.function.clone(),
                        entry,
                    }));
                }
                if commands.is_closed() {
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
    entry: Arc<StepEntry<FanCommand>>,
}
impl FanRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.entry.queued_at()
    }
    pub(crate) fn needs_power_on(&self) -> bool {
        let state = self.function.0.state.lock().unwrap();
        state.fan_commands().unwrap().needs_power_on()
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.entry.current_result()
    }
    pub(crate) fn mark_sent(&self, step: FanStep) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        self.entry.mark_sent()?;
        if step == FanStep::Power(false) {
            state.fan_commands_mut().unwrap().power_off_sent();
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self) -> Result<(), CommandError> {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.step_accepted()
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        self.entry.accept(
            || self.function.snapshot_locked(&state),
            "fan request has no settled write",
        )
    }
    pub(crate) fn reject(self, error: CommandError) {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.reject(error);
    }
}
impl Drop for FanRequest {
    fn drop(&mut self) {
        let _state = self.function.0.state.lock().unwrap();
        self.entry.cancel();
    }
}
