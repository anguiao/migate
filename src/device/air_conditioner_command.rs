use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

use super::{
    AirConditionerFanSpeed, AirConditionerMode, AirConditionerTarget, Command, CommandError,
    CommandReceiver, DeviceRequest, Function, FunctionKind, FunctionSnapshot,
    air_conditioner::{AirConditionerConstraints, ConstraintViolation},
    state::{AirConditionerRuntime, RuntimeState},
    step_command::{self, StepAction, StepCommand, StepCoordinator, StepRequest, StepSubmission},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AirConditionerCommand {
    Power(bool),
    Mode(AirConditionerMode),
    Temperature(i16),
    Speed(AirConditionerFanSpeed),
    Swing(bool),
    #[allow(
        dead_code,
        reason = "The domain supports explicit combined targets; current Matter and terminal callers submit individual commands"
    )]
    Combined(AirConditionerTarget),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AirConditionerStep {
    Power(bool),
    Mode(AirConditionerMode),
    Temperature(i16),
    Speed(AirConditionerFanSpeed),
    Swing(bool),
}

impl StepCommand for AirConditionerCommand {
    const MISSING_WRITE: &'static str = "air-conditioner request has no settled write";

    fn coordinator(state: &RuntimeState) -> Option<&StepCoordinator<Self>> {
        state.air_conditioner().map(|runtime| &runtime.commands)
    }
    fn coordinator_mut(state: &mut RuntimeState) -> Option<&mut StepCoordinator<Self>> {
        state
            .air_conditioner_mut()
            .map(|runtime| &mut runtime.commands)
    }
    fn supersedes(self, previous: Self) -> bool {
        matches!(
            (self, previous),
            (Self::Temperature(_), Self::Temperature(_))
                | (Self::Speed(_), Self::Speed(_))
                | (Self::Swing(_), Self::Swing(_))
        )
    }
    fn interrupts(self, previous: Self) -> bool {
        matches!(self, Self::Power(false)) && !matches!(previous, Self::Power(_))
    }
}
pub struct AirConditionerAction(StepAction<AirConditionerCommand>);
impl AirConditionerAction {
    pub async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
}

impl Function {
    pub fn begin_air_conditioner(
        &self,
        command: AirConditionerCommand,
    ) -> Result<super::AirConditionerAction, CommandError> {
        let FunctionKind::AirConditioner { vertical_swing, .. } = self.description().kind else {
            return Err(CommandError::Unsupported);
        };
        if !vertical_swing
            && (matches!(command, AirConditionerCommand::Swing(_))
                || matches!(command, AirConditionerCommand::Combined(target) if target.swing.is_some()))
        {
            return Err(CommandError::Unsupported);
        }
        step_command::submit::<AirConditionerCommand>(self, |_| Ok(command))
            .map(AirConditionerAction)
    }

    pub(super) async fn execute_air_conditioner_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = step_command::submit::<AirConditionerCommand>(self, |state| {
            Ok(AirConditionerCommand::Power(match command {
                Command::On => true,
                Command::Off => false,
                Command::Toggle => !state
                    .air_conditioner()
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

    pub(crate) fn set_air_conditioner_constraints(&self, constraints: AirConditionerConstraints) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state.air_conditioner_mut().unwrap();
        let FunctionKind::AirConditioner { temperature, .. } = self.description().kind else {
            unreachable!()
        };
        assert_eq!(constraints.grid(), temperature);
        runtime.constraints_current = true;
        runtime.constraints = Some(constraints);
    }
    pub(crate) fn require_air_conditioner_source_constraints(&self) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state.air_conditioner_mut().unwrap();
        runtime.dynamic_constraints = true;
        runtime.constraints = None;
        runtime.constraints_current = false;
    }
    pub(crate) fn air_conditioner_constraints_ready(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .air_conditioner()
            .unwrap()
            .constraints_current
    }
    pub(crate) fn air_conditioner_constraints(&self) -> Option<AirConditionerConstraints> {
        self.0
            .state
            .lock()
            .unwrap()
            .air_conditioner()
            .unwrap()
            .constraints
            .clone()
    }

    pub(crate) fn pending_air_conditioner_commands(&self) -> Vec<AirConditionerCommand> {
        let state = self.0.state.lock().unwrap();
        state
            .air_conditioner()
            .map_or_else(Vec::new, |runtime| runtime.commands.pending_commands())
    }
}

impl CommandReceiver {
    pub(super) async fn next_air_conditioner_operation(&mut self) -> Option<DeviceRequest> {
        let request = step_command::next::<AirConditionerCommand>(&self.function).await?;
        let command = request.entry.command;
        Some(DeviceRequest::AirConditioner(AirConditionerRequest {
            command,
            request,
            mode_revision: AtomicU64::new(0),
            mode_pending: AtomicBool::new(false),
        }))
    }
}

pub struct AirConditionerRequest {
    pub command: AirConditionerCommand,
    request: StepRequest<AirConditionerCommand>,
    mode_revision: AtomicU64,
    mode_pending: AtomicBool,
}
impl AirConditionerRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.request.queued_at()
    }
    pub(crate) fn plan(&self) -> Result<Vec<AirConditionerStep>, CommandError> {
        let state = self.request.function.0.state.lock().unwrap();
        let runtime = state.air_conditioner().unwrap();
        self.plan_with_runtime(runtime)
    }
    fn plan_with_runtime(
        &self,
        runtime: &AirConditionerRuntime,
    ) -> Result<Vec<AirConditionerStep>, CommandError> {
        let target = match self.command {
            AirConditionerCommand::Power(value) => {
                return Ok(vec![AirConditionerStep::Power(value)]);
            }
            AirConditionerCommand::Swing(value) => {
                return Ok(vec![AirConditionerStep::Swing(value)]);
            }
            AirConditionerCommand::Mode(mode) => AirConditionerTarget {
                mode: Some(mode),
                ..Default::default()
            },
            AirConditionerCommand::Temperature(temperature) => AirConditionerTarget {
                temperature: Some(temperature),
                ..Default::default()
            },
            AirConditionerCommand::Speed(speed) => AirConditionerTarget {
                speed: Some(speed),
                ..Default::default()
            },
            AirConditionerCommand::Combined(target) => target,
        };
        if target.mode.is_none() && target.temperature.is_none() && target.speed.is_none() {
            return target
                .swing
                .map(|swing| vec![AirConditionerStep::Swing(swing)])
                .ok_or(CommandError::Unsupported);
        }
        validate_target(runtime, target)?;
        let mut steps = Vec::new();
        if (target.mode.is_some() || target.speed.is_some())
            && runtime.power_confirmation.needs_power_on()
        {
            steps.push(AirConditionerStep::Power(true));
        }
        if let Some(mode) = target.mode {
            steps.push(AirConditionerStep::Mode(mode));
        }
        if let Some(temperature) = target.temperature {
            steps.push(AirConditionerStep::Temperature(temperature));
        }
        if let Some(speed) = target.speed {
            steps.push(AirConditionerStep::Speed(speed));
        }
        if let Some(swing) = target.swing {
            steps.push(AirConditionerStep::Swing(swing));
        }
        Ok(steps)
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.request.current_result()
    }
    fn validate_step(
        &self,
        runtime: &AirConditionerRuntime,
        step: AirConditionerStep,
    ) -> Result<(), CommandError> {
        if !self.request.entry.started() {
            self.plan_with_runtime(runtime)?;
            return Ok(());
        }
        let target = match step {
            AirConditionerStep::Mode(mode) => AirConditionerTarget {
                mode: Some(mode),
                ..Default::default()
            },
            AirConditionerStep::Temperature(temperature) => AirConditionerTarget {
                temperature: Some(temperature),
                ..Default::default()
            },
            AirConditionerStep::Speed(speed) => AirConditionerTarget {
                speed: Some(speed),
                ..Default::default()
            },
            AirConditionerStep::Power(_) | AirConditionerStep::Swing(_) => return Ok(()),
        };
        validate_target(runtime, target)
    }
    pub(crate) fn send_result(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let state = self.request.function.0.state.lock().unwrap();
        self.request.current_result()?;
        self.validate_step(state.air_conditioner().unwrap(), step)
    }
    pub(crate) fn mark_sent(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let mut state = self.request.function.0.state.lock().unwrap();
        self.request.current_result()?;
        self.validate_step(state.air_conditioner().unwrap(), step)?;
        self.request.mark_sent()?;
        let runtime = state.air_conditioner_mut().unwrap();
        if step == AirConditionerStep::Power(false) {
            runtime.power_confirmation.power_off_sent();
        }
        if matches!(step, AirConditionerStep::Mode(_)) {
            self.mode_revision
                .store(runtime.mode_revision, Ordering::Relaxed);
            self.mode_pending.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let mut state = self.request.function.0.state.lock().unwrap();
        self.request.step_accepted()?;
        if let AirConditionerStep::Mode(mode) = step {
            self.mode_pending.store(false, Ordering::Relaxed);
            let runtime = state.air_conditioner_mut().unwrap();
            if runtime.mode_revision == self.mode_revision.load(Ordering::Relaxed) {
                runtime.planning_mode = Some(mode);
            }
        }
        Ok(())
    }
    pub(crate) fn accept(self) -> Result<FunctionSnapshot, CommandError> {
        self.request.accept()
    }
    pub(crate) fn reject(self, error: CommandError) {
        let mut state = self.request.function.0.state.lock().unwrap();
        if self.mode_pending.swap(false, Ordering::Relaxed) {
            state.air_conditioner_mut().unwrap().planning_mode = None;
        }
        drop(state);
        self.request.reject(error);
    }
}

fn validate_target(
    runtime: &AirConditionerRuntime,
    target: AirConditionerTarget,
) -> Result<(), CommandError> {
    let constraints = runtime
        .constraints
        .as_ref()
        .filter(|_| runtime.constraints_current)
        .ok_or(CommandError::UnknownState)?;
    let map = |violation| match violation {
        ConstraintViolation::Constraint => CommandError::Constraint,
        ConstraintViolation::InvalidInState => CommandError::InvalidInState,
    };
    if target
        .temperature
        .is_some_and(|value| !constraints.grid().contains(value))
    {
        return Err(CommandError::Constraint);
    }
    let mode = target
        .mode
        .or(runtime.planning_mode)
        .or(runtime.observed_mode);
    if let Some(mode) = target.mode {
        constraints.validate_mode(mode).map_err(map)?;
    }
    if let Some(temperature) = target.temperature {
        constraints
            .validate_temperature(mode.ok_or(CommandError::UnknownState)?, temperature)
            .map_err(map)?;
    }
    if let Some(speed) = target.speed {
        constraints
            .validate_speed(mode.ok_or(CommandError::UnknownState)?, speed)
            .map_err(map)?;
    }
    Ok(())
}
impl Drop for AirConditionerRequest {
    fn drop(&mut self) {
        let mut state = self.request.function.0.state.lock().unwrap();
        if self.mode_pending.swap(false, Ordering::Relaxed) {
            state.air_conditioner_mut().unwrap().planning_mode = None;
        }
    }
}
