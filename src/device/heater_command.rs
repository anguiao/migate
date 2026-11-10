use std::time::Instant;

use super::{
    Command, CommandError, CommandReceiver, DeviceRequest, Function, FunctionKind,
    FunctionSnapshot, HeaterMode,
    thermostat::{self, TemperatureGrid},
    thermostat::{
        HeaterProfile, ThermostatAction, ThermostatEntry, ThermostatPowerSubmission,
        ThermostatProfile,
    },
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

impl super::step_command::StepCommand for HeaterCommand {
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

pub struct HeaterAction(ThermostatAction<HeaterCommand>);
impl HeaterAction {
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

impl Function {
    pub fn begin_heater(&self, command: HeaterCommand) -> Result<HeaterAction, CommandError> {
        thermostat::submit::<HeaterProfile>(self, |_| Ok(command)).map(HeaterAction)
    }

    pub(super) async fn execute_heater_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = thermostat::submit::<HeaterProfile>(self, |runtime| {
            Ok(HeaterCommand::Power(match command {
                Command::On => true,
                Command::Off => false,
                Command::Toggle => !runtime
                    .observation
                    .power
                    .value
                    .ok_or(CommandError::UnknownState)?,
            }))
        })?;
        let _submission =
            ThermostatPowerSubmission::<HeaterProfile>::new(self.clone(), action.entry().clone());
        action.accepted().await
    }

    pub(crate) fn pending_heater_commands(&self) -> Vec<HeaterCommand> {
        let state = self.0.state.lock().unwrap();
        state
            .heater()
            .map_or_else(Vec::new, |runtime| runtime.thermostat.pending_commands())
    }
}

impl CommandReceiver {
    pub(super) async fn next_heater_operation(&mut self) -> Option<DeviceRequest> {
        let entry = thermostat::next::<HeaterProfile>(&self.function).await?;
        let command = entry.command;
        Some(DeviceRequest::Heater(HeaterRequest {
            command,
            entry: ThermostatEntry::new(self.function.clone(), entry, HeaterProfile::MISSING_WRITE),
        }))
    }
}

pub struct HeaterRequest {
    pub command: HeaterCommand,
    entry: ThermostatEntry<HeaterCommand>,
}
impl HeaterRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.entry.queued_at()
    }
    pub(crate) fn plan(&self) -> Result<Vec<HeaterStep>, CommandError> {
        let grid = grid(self.entry.function())?;
        Ok(match self.command {
            HeaterCommand::Power(value) => vec![HeaterStep::Power(value)],
            HeaterCommand::Mode(HeaterMode::Off) => vec![HeaterStep::Power(false)],
            HeaterCommand::Mode(HeaterMode::Heat) => vec![HeaterStep::Power(true)],
            HeaterCommand::Temperature(value) => {
                if !grid.contains(value) {
                    return Err(CommandError::Constraint);
                }
                vec![HeaterStep::Temperature(value)]
            }
        })
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.entry.current_result()
    }
    pub(crate) fn send_result(&self, step: HeaterStep) -> Result<(), CommandError> {
        self.entry.current_result()?;
        let grid = grid(self.entry.function())?;
        match step {
            HeaterStep::Temperature(value) if !grid.contains(value) => {
                Err(CommandError::Constraint)
            }
            HeaterStep::Temperature(_) | HeaterStep::Power(_) => Ok(()),
        }
    }
    pub(crate) fn mark_sent(&self, step: HeaterStep) -> Result<(), CommandError> {
        self.entry.current_result()?;
        let grid = grid(self.entry.function())?;
        if let HeaterStep::Temperature(value) = step
            && !grid.contains(value)
        {
            return Err(CommandError::Constraint);
        }
        self.entry.mark_sent()?;
        if step == HeaterStep::Power(false) {
            let mut state = self.entry.function().0.state.lock().unwrap();
            state
                .heater_mut()
                .unwrap()
                .thermostat
                .commands
                .power_off_sent();
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self, _step: HeaterStep) -> Result<(), CommandError> {
        self.entry.step_accepted()
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        self.entry.accept()
    }
    pub(crate) fn reject(self, error: CommandError) {
        self.entry.reject(error)
    }
}
impl Drop for HeaterRequest {
    fn drop(&mut self) {
        self.entry.cancel();
    }
}

fn grid(function: &Function) -> Result<TemperatureGrid, CommandError> {
    match function.description().kind {
        FunctionKind::Heater { temperature } => Ok(temperature),
        _ => Err(CommandError::Unsupported),
    }
}
