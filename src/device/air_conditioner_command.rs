use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use super::{
    AirConditionerFanSpeed, AirConditionerMode, AirConditionerTarget, Command, CommandError,
    CommandReceiver, DeviceRequest, Function, FunctionKind, FunctionSnapshot,
    air_conditioner::{AirConditionerConstraints, ConstraintViolation},
    step_command::{StepCommand, StepCoordinator, StepEntry},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AirConditionerCommand {
    Power(bool),
    Mode(AirConditionerMode),
    Temperature(i16),
    Speed(AirConditionerFanSpeed),
    Swing(bool),
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
pub(super) type AirConditionerCoordinator = StepCoordinator<AirConditionerCommand>;

pub struct AirConditionerAction(Arc<StepEntry<AirConditionerCommand>>);
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
        let mut state = self.0.state.lock().unwrap();
        let runtime = state.air_conditioner().unwrap();
        if runtime.commands.is_closed() {
            return Err(CommandError::Cancelled);
        }
        self.snapshot_locked(&state).availability.command_result()?;
        let action = AirConditionerAction(
            state
                .air_conditioner_mut()
                .unwrap()
                .commands
                .submit(command),
        );
        self.0.submitted.notify(usize::MAX);
        Ok(action)
    }

    pub(super) async fn execute_air_conditioner_power(
        &self,
        command: Command,
    ) -> Result<FunctionSnapshot, CommandError> {
        let action = match command {
            Command::On => self.begin_air_conditioner(AirConditionerCommand::Power(true))?,
            Command::Off => self.begin_air_conditioner(AirConditionerCommand::Power(false))?,
            Command::Toggle => {
                let mut state = self.0.state.lock().unwrap();
                let runtime = state.air_conditioner().unwrap();
                if runtime.commands.is_closed() {
                    return Err(CommandError::Cancelled);
                }
                self.snapshot_locked(&state).availability.command_result()?;
                let value = !runtime
                    .snapshot
                    .power
                    .value
                    .ok_or(CommandError::UnknownState)?;
                let action = AirConditionerAction(
                    state
                        .air_conditioner_mut()
                        .unwrap()
                        .commands
                        .submit(AirConditionerCommand::Power(value)),
                );
                self.0.submitted.notify(usize::MAX);
                action
            }
        };
        let _submission = PowerSubmission {
            function: self.clone(),
            entry: action.0.clone(),
        };
        action.accepted().await
    }

    pub(crate) fn set_air_conditioner_constraints(
        &self,
        constraints: Option<AirConditionerConstraints>,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state.air_conditioner_mut().unwrap();
        if let Some(constraints) = &constraints {
            let FunctionKind::AirConditioner { temperature, .. } = self.description().kind else {
                unreachable!()
            };
            assert_eq!(constraints.grid(), temperature);
        }
        runtime.constraints_current = constraints.is_some();
        runtime.constraints = constraints;
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

struct PowerSubmission {
    function: Function,
    entry: Arc<StepEntry<AirConditionerCommand>>,
}
impl Drop for PowerSubmission {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        state
            .air_conditioner_mut()
            .unwrap()
            .commands
            .cancel_pending(&self.entry);
    }
}

impl CommandReceiver {
    pub(super) async fn next_air_conditioner_operation(&mut self) -> Option<DeviceRequest> {
        let flight = self
            .function
            .0
            .state
            .lock()
            .unwrap()
            .air_conditioner()
            .unwrap()
            .commands
            .flight();
        if let Some(flight) = flight {
            flight.wait_gate().await;
            self.function
                .0
                .state
                .lock()
                .unwrap()
                .air_conditioner_mut()
                .unwrap()
                .commands
                .release_flight(&flight);
        }
        loop {
            let listener = self.function.0.submitted.listen();
            {
                let mut state = self.function.0.state.lock().unwrap();
                let commands = &mut state.air_conditioner_mut().unwrap().commands;
                if let Some(entry) = commands.take_next() {
                    return Some(DeviceRequest::AirConditioner(AirConditionerRequest {
                        command: entry.command,
                        function: self.function.clone(),
                        entry,
                        mode_revision: AtomicU64::new(0),
                        mode_pending: AtomicBool::new(false),
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

pub struct AirConditionerRequest {
    pub command: AirConditionerCommand,
    function: Function,
    entry: Arc<StepEntry<AirConditionerCommand>>,
    mode_revision: AtomicU64,
    mode_pending: AtomicBool,
}
impl AirConditionerRequest {
    pub(crate) fn queued_at(&self) -> Instant {
        self.entry.queued_at()
    }
    pub(crate) fn plan(&self) -> Result<Vec<AirConditionerStep>, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        let runtime = state.air_conditioner().unwrap();
        self.plan_with_runtime(runtime)
    }
    fn plan_with_runtime(
        &self,
        runtime: &super::state::AirConditionerRuntime,
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
        let mut steps = Vec::new();
        if (target.mode.is_some() || target.speed.is_some()) && runtime.commands.needs_power_on() {
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
        if steps.is_empty() {
            return Err(CommandError::Unsupported);
        }
        Ok(steps)
    }
    pub(crate) fn current_result(&self) -> Result<(), CommandError> {
        self.entry.current_result()
    }
    fn validate_step(
        &self,
        runtime: &super::state::AirConditionerRuntime,
        step: AirConditionerStep,
    ) -> Result<(), CommandError> {
        if !self.entry.started() {
            self.plan_with_runtime(runtime)?;
            return Ok(());
        }
        if matches!(
            step,
            AirConditionerStep::Power(_) | AirConditionerStep::Swing(_)
        ) {
            return Ok(());
        }
        let constraints = runtime
            .constraints
            .as_ref()
            .filter(|_| runtime.constraints_current)
            .ok_or(CommandError::UnknownState)?;
        let map = |violation| match violation {
            ConstraintViolation::Constraint => CommandError::Constraint,
            ConstraintViolation::InvalidInState => CommandError::InvalidInState,
        };
        match step {
            AirConditionerStep::Mode(mode) => constraints.validate_mode(mode).map_err(map),
            AirConditionerStep::Temperature(temperature) => {
                if !constraints.grid().contains(temperature) {
                    return Err(CommandError::Constraint);
                }
                let mode = runtime
                    .planning_mode
                    .or(runtime.observed_mode)
                    .ok_or(CommandError::UnknownState)?;
                constraints
                    .validate_temperature(mode, temperature)
                    .map_err(map)
            }
            AirConditionerStep::Speed(speed) => {
                let mode = runtime
                    .planning_mode
                    .or(runtime.observed_mode)
                    .ok_or(CommandError::UnknownState)?;
                constraints.validate_speed(mode, speed).map_err(map)
            }
            AirConditionerStep::Power(_) | AirConditionerStep::Swing(_) => unreachable!(),
        }
    }
    pub(crate) fn send_result(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let state = self.function.0.state.lock().unwrap();
        self.entry.current_result()?;
        self.validate_step(state.air_conditioner().unwrap(), step)
    }
    pub(crate) fn mark_sent(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        self.entry.current_result()?;
        self.validate_step(state.air_conditioner().unwrap(), step)?;
        self.entry.mark_sent()?;
        let runtime = state.air_conditioner_mut().unwrap();
        if step == AirConditionerStep::Power(false) {
            runtime.commands.power_off_sent();
        }
        if matches!(step, AirConditionerStep::Mode(_)) {
            self.mode_revision
                .store(runtime.mode_revision, Ordering::Relaxed);
            self.mode_pending.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
    pub(crate) fn step_accepted(&self, step: AirConditionerStep) -> Result<(), CommandError> {
        let mut state = self.function.0.state.lock().unwrap();
        self.entry.step_accepted()?;
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
        let state = self.function.0.state.lock().unwrap();
        self.entry.accept(
            || self.function.snapshot_locked(&state),
            "air-conditioner request has no settled write",
        )
    }
    pub(crate) fn reject(self, error: CommandError) {
        let mut state = self.function.0.state.lock().unwrap();
        if self.mode_pending.swap(false, Ordering::Relaxed) {
            state.air_conditioner_mut().unwrap().planning_mode = None;
        }
        self.entry.reject(error);
    }
}
impl Drop for AirConditionerRequest {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        if self.mode_pending.swap(false, Ordering::Relaxed) {
            state.air_conditioner_mut().unwrap().planning_mode = None;
        }
        self.entry.cancel();
    }
}
