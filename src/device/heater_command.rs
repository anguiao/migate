use std::time::Instant;

use super::{
    Command, CommandError, CommandReceiver, DeviceRequest, Function, FunctionKind,
    FunctionSnapshot, HeaterMode,
    state::RuntimeState,
    step_command::{self, StepAction, StepCommand, StepCoordinator, StepRequest, StepSubmission},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaterCommand {
    Power(bool),
    Mode(HeaterMode),
    Temperature(i16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeaterStep {
    Power(bool),
    Temperature(i16),
}

impl StepCommand for HeaterCommand {
    const MISSING_WRITE: &'static str = "heater request has no settled write";

    fn coordinator(state: &RuntimeState) -> Option<&StepCoordinator<Self>> {
        state.heater().map(|runtime| &runtime.commands)
    }
    fn coordinator_mut(state: &mut RuntimeState) -> Option<&mut StepCoordinator<Self>> {
        state.heater_mut().map(|runtime| &mut runtime.commands)
    }
    fn supersedes(self, previous: Self) -> bool {
        matches!(
            (self, previous),
            (Self::Temperature(_), Self::Temperature(_))
        )
    }
    fn interrupts(self, previous: Self) -> bool {
        let off = matches!(self, Self::Power(false) | Self::Mode(HeaterMode::Off));
        off && !matches!(previous, Self::Power(_) | Self::Mode(_))
    }
}

pub struct HeaterAction(StepAction<HeaterCommand>);
impl HeaterAction {
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

impl Function {
    pub fn begin_heater(&self, command: HeaterCommand) -> Result<HeaterAction, CommandError> {
        step_command::submit::<HeaterCommand>(self, |_| {
            if let (HeaterCommand::Temperature(value), FunctionKind::Heater { temperature }) =
                (command, &self.description().kind)
                && !temperature.contains(value)
            {
                return Err(CommandError::Constraint);
            }
            Ok(command)
        })
        .map(HeaterAction)
    }

    pub(super) async fn execute_heater_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = step_command::submit::<HeaterCommand>(self, |state| {
            Ok(HeaterCommand::Power(match command {
                Command::On => true,
                Command::Off => false,
                Command::Toggle => !state
                    .heater()
                    .unwrap()
                    .thermostat
                    .power
                    .value
                    .ok_or(CommandError::UnknownState)?,
            }))
        })?;
        let _submission = StepSubmission::new(self.clone(), action.0.clone());
        action.accepted().await
    }

    pub(crate) fn pending_heater_commands(&self) -> Vec<HeaterCommand> {
        let state = self.0.state.lock().unwrap();
        state
            .heater()
            .map_or_else(Vec::new, |runtime| runtime.commands.pending_commands())
    }
}

impl CommandReceiver {
    pub(super) async fn next_heater_operation(&mut self) -> Option<DeviceRequest> {
        let request = step_command::next::<HeaterCommand>(&self.function).await?;
        let command = request.entry.command;
        Some(DeviceRequest::Heater(HeaterRequest { command, request }))
    }
}

pub struct HeaterRequest {
    pub command: HeaterCommand,
    request: StepRequest<HeaterCommand>,
}
impl HeaterRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.request.queued_at()
    }
    pub(crate) fn step(&self) -> HeaterStep {
        match self.command {
            HeaterCommand::Power(value) => HeaterStep::Power(value),
            HeaterCommand::Mode(mode) => HeaterStep::Power(mode == HeaterMode::Heat),
            HeaterCommand::Temperature(value) => HeaterStep::Temperature(value),
        }
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }
    pub(crate) fn mark_sent(&self) -> Result<(), CommandError> {
        let _state = self.request.function.0.state.lock().unwrap();
        self.request.mark_sent()
    }
    pub(crate) fn step_accepted(&self) -> Result<(), CommandError> {
        let _state = self.request.function.0.state.lock().unwrap();
        self.request.step_accepted()
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        self.request.accept()
    }
    pub(crate) fn reject(self, error: CommandError) {
        self.request.reject(error)
    }
}
