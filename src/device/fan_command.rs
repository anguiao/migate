use std::{sync::Arc, time::Instant};

use super::{
    Command, CommandError, CommandReceiver, DeviceRequest, Function, FunctionKind,
    FunctionSnapshot,
    state::RuntimeState,
    step_command::{
        self, StepAction, StepCommand, StepCoordinator, StepEntry, StepRequest, StepSubmission,
    },
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
    const MISSING_WRITE: &'static str = "fan request has no settled write";

    fn coordinator(state: &RuntimeState) -> Option<&StepCoordinator<Self>> {
        state.fan().map(|runtime| &runtime.commands)
    }
    fn coordinator_mut(state: &mut RuntimeState) -> Option<&mut StepCoordinator<Self>> {
        state.fan_mut().map(|runtime| &mut runtime.commands)
    }
    fn supersedes(self, previous: Self) -> bool {
        self.is_speed() && previous.is_speed()
    }

    fn interrupts(self, previous: Self) -> bool {
        self.is_off() && previous.is_speed()
    }

    fn follow(self, previous: &Arc<StepEntry<Self>>) -> Option<Arc<StepEntry<Self>>> {
        if self != Self::Power(true) {
            return None;
        }
        match previous.command {
            Self::Speed(1) => Some(previous.clone()),
            Self::Power(true) => previous.preceding.clone(),
            _ => None,
        }
    }
}

pub struct FanAction(StepAction<FanCommand>);
impl FanAction {
    /// A successful result means all required writes were accepted.
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

impl Function {
    pub(crate) fn pending_fan_commands(&self) -> Vec<FanCommand> {
        let state = self.0.state.lock().unwrap();
        state
            .fan()
            .map_or_else(Vec::new, |runtime| runtime.commands.pending_commands())
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
        step_command::submit(self, |_| Ok(command)).map(FanAction)
    }

    pub(super) async fn execute_fan_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = step_command::submit::<FanCommand>(self, |state| {
            Ok(FanCommand::Power(match command {
                Command::On => true,
                Command::Off => false,
                Command::Toggle => !state
                    .fan()
                    .unwrap()
                    .snapshot
                    .power
                    .value
                    .ok_or(CommandError::UnknownState)?,
            }))
        })?;
        let _submission = StepSubmission::new(self.clone(), action.0.clone());
        action.accepted().await
    }
}

impl CommandReceiver {
    pub(super) async fn next_fan_operation(&mut self) -> Option<DeviceRequest> {
        let request = step_command::next::<FanCommand>(&self.function).await?;
        Some(DeviceRequest::Fan(FanRequest {
            command: request.entry.command,
            request,
        }))
    }
}

/// Owns every step of one operation until the original receipt settles.
pub struct FanRequest {
    pub command: FanCommand,
    request: StepRequest<FanCommand>,
}
impl FanRequest {
    pub(crate) fn unsent_low_failure(&self) -> Option<CommandError> {
        self.request.entry.preceding.as_ref()?.unsent_failure()
    }
    pub(crate) fn queued_at(&self) -> Instant {
        self.request.queued_at()
    }
    pub(crate) fn needs_power_on(&self) -> bool {
        let state = self.request.function.0.state.lock().unwrap();
        state.fan().unwrap().power_confirmation.needs_power_on()
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }
    pub(crate) fn mark_sent(&self, step: FanStep) -> Result<(), CommandError> {
        let mut state = self.request.function.0.state.lock().unwrap();
        self.request.mark_sent()?;
        if step == FanStep::Power(false) {
            state.fan_mut().unwrap().power_confirmation.power_off_sent();
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self) -> Result<(), CommandError> {
        let _state = self.request.function.0.state.lock().unwrap();
        self.request.step_accepted()
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        self.request.accept()
    }
    pub(crate) fn accept_unchanged(self) -> Result<FunctionSnapshot, CommandError> {
        self.request.accept_unchanged()
    }
    pub(crate) fn reject(self, error: CommandError) {
        self.request.reject(error);
    }
}
