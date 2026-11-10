//! Source-independent thermostat primitives shared by air conditioners and
//! heating-only functions.

use std::{sync::Arc, time::Instant};

use super::{
    AirConditionerCommand, CommandError, Function, FunctionSnapshot, HeaterCommand,
    MeasurementSnapshot, TemperatureSourceState,
    model::PropertyObservation,
    state::RuntimeState,
    step_command::{StepCommand, StepEntry},
};

/// A native grid in whole degrees Celsius, representable in Matter centidegrees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemperatureGrid {
    min: i16,
    max: i16,
    step: i16,
}
impl TemperatureGrid {
    pub fn new(min: i16, max: i16, step: i16) -> Result<Self, &'static str> {
        if min < -273 || max > 327 || min > max || step <= 0 {
            return Err("invalid thermostat temperature grid");
        }
        Ok(Self { min, max, step })
    }
    pub(crate) fn min(self) -> i16 {
        self.min
    }
    pub(crate) fn max(self) -> i16 {
        self.max
    }
    pub(crate) fn step(self) -> i16 {
        self.step
    }
    pub(crate) fn effective_max(self) -> i16 {
        let min = i32::from(self.min);
        let step = i32::from(self.step);
        (min + (i32::from(self.max) - min) / step * step) as i16
    }
    pub(crate) fn contains(self, value: i16) -> bool {
        (self.min..=self.max).contains(&value)
            && (i32::from(value) - i32::from(self.min)) % i32::from(self.step) == 0
    }
    pub(crate) fn values(self) -> impl Iterator<Item = i16> {
        (i32::from(self.min)..=i32::from(self.max))
            .step_by(self.step as usize)
            .map(|value| value as i16)
    }
}

/// Shared Power/Mode/Target-temperature observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ThermostatObservation<M> {
    pub power: PropertyObservation<bool>,
    pub mode: PropertyObservation<M>,
    pub target_temperature: PropertyObservation<i16>,
    pub current_temperature: TemperatureSourceRuntime,
}
impl<M> Default for ThermostatObservation<M> {
    fn default() -> Self {
        Self {
            power: PropertyObservation::default(),
            mode: PropertyObservation::default(),
            target_temperature: PropertyObservation::default(),
            current_temperature: TemperatureSourceRuntime::default(),
        }
    }
}

/// One built-in temperature source with the same validity semantics as measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TemperatureSourceRuntime {
    pub measurement: MeasurementSnapshot,
    pub source: TemperatureSourceState,
}
impl Default for TemperatureSourceRuntime {
    fn default() -> Self {
        Self {
            measurement: MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            },
            source: TemperatureSourceState::Unavailable,
        }
    }
}
impl TemperatureSourceRuntime {
    pub(super) fn set_available(&mut self, available: bool) -> bool {
        set_temperature_source_available(&mut self.source, available)
    }
    pub(super) fn observe(
        &mut self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        observe_temperature(
            &mut self.measurement,
            &mut self.source,
            value,
            source_time,
            received_at,
        )
    }
}

pub(super) fn set_temperature_source_available(
    source: &mut TemperatureSourceState,
    available: bool,
) -> bool {
    let next = if available {
        match *source {
            TemperatureSourceState::Unavailable => TemperatureSourceState::AwaitingReading,
            current => current,
        }
    } else {
        TemperatureSourceState::Unavailable
    };
    if *source == next {
        false
    } else {
        *source = next;
        true
    }
}

pub(super) fn invalidate_temperature_reading(
    measurement: &mut MeasurementSnapshot,
    source: &mut TemperatureSourceState,
    source_time: Option<i64>,
    received_at: i64,
) -> bool {
    let observed = MeasurementSnapshot {
        value: None,
        source_time,
        received_at: Some(received_at),
    };
    let next = match *source {
        TemperatureSourceState::Available(_) | TemperatureSourceState::AwaitingReading => {
            TemperatureSourceState::AwaitingReading
        }
        other => other,
    };
    if *measurement == observed && *source == next {
        false
    } else {
        *measurement = observed;
        *source = next;
        true
    }
}

pub(super) fn observe_temperature(
    measurement: &mut MeasurementSnapshot,
    source: &mut TemperatureSourceState,
    value: f64,
    source_time: Option<i64>,
    received_at: i64,
) -> bool {
    let observed = MeasurementSnapshot {
        value: Some(value),
        source_time,
        received_at: Some(received_at),
    };
    let next = match *source {
        TemperatureSourceState::AwaitingReading | TemperatureSourceState::Available(_) => {
            TemperatureSourceState::Available(observed)
        }
        other => other,
    };
    if *measurement == observed && *source == next {
        false
    } else {
        *measurement = observed;
        *source = next;
        true
    }
}

/// The command and observation core shared by every thermostat profile.
pub(super) struct ThermostatRuntime<C, M> {
    pub observation: ThermostatObservation<M>,
    pub commands: super::step_command::StepCoordinator<C>,
}
impl<C: StepCommand, M> Default for ThermostatRuntime<C, M> {
    fn default() -> Self {
        Self {
            observation: ThermostatObservation::default(),
            commands: super::step_command::StepCoordinator::default(),
        }
    }
}
impl<C: StepCommand, M: Copy + PartialEq> ThermostatRuntime<C, M> {
    pub(super) fn observe_power(
        &mut self,
        power: bool,
        fresh: bool,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        self.commands.observe_power(power, fresh);
        self.observation
            .power
            .update(power, source_time, received_at)
    }
    pub(super) fn observe_mode(
        &mut self,
        mode: M,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        self.observation.mode.update(mode, source_time, received_at)
    }
    pub(super) fn observe_target_temperature(
        &mut self,
        temperature: i16,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        self.observation
            .target_temperature
            .update(temperature, source_time, received_at)
    }
    pub(super) fn set_temperature_source_available(&mut self, available: bool) -> bool {
        self.observation
            .current_temperature
            .set_available(available)
    }
    pub(super) fn observe_current_temperature(
        &mut self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        self.observation
            .current_temperature
            .observe(value, source_time, received_at)
    }
    pub(super) fn observe_invalid_current_temperature(
        &mut self,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        invalidate_temperature_reading(
            &mut self.observation.current_temperature.measurement,
            &mut self.observation.current_temperature.source,
            source_time,
            received_at,
        )
    }
    pub(super) fn pending_commands(&self) -> Vec<C> {
        self.commands.pending_commands()
    }
}

/// A valid thermostat profile: its full command set and runtime location.
pub(super) trait ThermostatProfile: Sized + 'static {
    type Command: StepCommand;
    type Mode: Copy + Eq + std::fmt::Debug;
    const MISSING_WRITE: &'static str;

    fn supports(function: &Function) -> bool;
    fn runtime(state: &RuntimeState) -> Option<&ThermostatRuntime<Self::Command, Self::Mode>>;
    fn runtime_mut(
        state: &mut RuntimeState,
    ) -> Option<&mut ThermostatRuntime<Self::Command, Self::Mode>>;
}

pub(super) struct AirConditionerProfile;
impl ThermostatProfile for AirConditionerProfile {
    type Command = AirConditionerCommand;
    type Mode = super::AirConditionerMode;
    const MISSING_WRITE: &'static str = "air-conditioner request has no settled write";

    fn supports(function: &Function) -> bool {
        matches!(
            function.description().kind,
            super::FunctionKind::AirConditioner { .. }
        )
    }
    fn runtime(state: &RuntimeState) -> Option<&ThermostatRuntime<Self::Command, Self::Mode>> {
        state.air_conditioner().map(|runtime| &runtime.thermostat)
    }
    fn runtime_mut(
        state: &mut RuntimeState,
    ) -> Option<&mut ThermostatRuntime<Self::Command, Self::Mode>> {
        state
            .air_conditioner_mut()
            .map(|runtime| &mut runtime.thermostat)
    }
}

pub(super) struct HeaterProfile;
impl ThermostatProfile for HeaterProfile {
    type Command = HeaterCommand;
    type Mode = super::HeaterMode;
    const MISSING_WRITE: &'static str = "heater request has no settled write";

    fn supports(function: &Function) -> bool {
        matches!(
            function.description().kind,
            super::FunctionKind::Heater { .. }
        )
    }
    fn runtime(state: &RuntimeState) -> Option<&ThermostatRuntime<Self::Command, Self::Mode>> {
        state.heater().map(|runtime| &runtime.thermostat)
    }
    fn runtime_mut(
        state: &mut RuntimeState,
    ) -> Option<&mut ThermostatRuntime<Self::Command, Self::Mode>> {
        state.heater_mut().map(|runtime| &mut runtime.thermostat)
    }
}

pub(super) struct ThermostatAction<C: StepCommand>(Arc<StepEntry<C>>);
impl<C: StepCommand> ThermostatAction<C> {
    pub(super) async fn accepted(&self) -> Result<FunctionSnapshot, CommandError> {
        self.0.accepted().await
    }
    pub(super) fn entry(&self) -> &Arc<StepEntry<C>> {
        &self.0
    }
}

/// The common submission boundary for both profiles.
pub(super) fn submit<P: ThermostatProfile>(
    function: &Function,
    resolve: impl FnOnce(&ThermostatRuntime<P::Command, P::Mode>) -> Result<P::Command, CommandError>,
) -> Result<ThermostatAction<P::Command>, CommandError> {
    if !P::supports(function) {
        return Err(CommandError::Unsupported);
    }
    let mut state = function.0.state.lock().unwrap();
    let runtime = P::runtime(&state).expect("validated thermostat runtime");
    if runtime.commands.is_closed() {
        return Err(CommandError::Cancelled);
    }
    function
        .snapshot_locked(&state)
        .availability
        .command_result()?;
    let command = resolve(runtime)?;
    let action = ThermostatAction(
        P::runtime_mut(&mut state)
            .expect("validated thermostat runtime")
            .commands
            .submit(command),
    );
    function.0.submitted.notify(usize::MAX);
    Ok(action)
}

/// Wait for the previous original receipt and take the next shared queue entry.
pub(super) async fn next<P: ThermostatProfile>(
    function: &Function,
) -> Option<Arc<StepEntry<P::Command>>> {
    let flight = P::runtime(&function.0.state.lock().unwrap())
        .expect("thermostat function has a runtime")
        .commands
        .flight();
    if let Some(flight) = flight {
        flight.wait_gate().await;
        P::runtime_mut(&mut function.0.state.lock().unwrap())
            .expect("thermostat function has a runtime")
            .commands
            .release_flight(&flight);
    }
    loop {
        let listener = function.0.submitted.listen();
        {
            let mut state = function.0.state.lock().unwrap();
            let runtime = P::runtime_mut(&mut state).expect("thermostat function has a runtime");
            if let Some(entry) = runtime.commands.take_next() {
                return Some(entry);
            }
            if runtime.commands.is_closed() {
                return None;
            }
        }
        listener.await;
    }
}

/// A request handle over one common queue entry.
pub(super) struct ThermostatEntry<C: StepCommand> {
    function: Function,
    entry: Arc<StepEntry<C>>,
    missing_write: &'static str,
}
impl<C: StepCommand> ThermostatEntry<C> {
    pub(super) fn new(
        function: Function,
        entry: Arc<StepEntry<C>>,
        missing_write: &'static str,
    ) -> Self {
        Self {
            function,
            entry,
            missing_write,
        }
    }
    pub(super) fn queued_at(&self) -> Instant {
        self.entry.queued_at()
    }
    pub(super) fn current_result(&self) -> Result<(), CommandError> {
        self.entry.current_result()
    }
    pub(super) fn mark_sent(&self) -> Result<(), CommandError> {
        self.entry.mark_sent()
    }
    pub(super) fn step_accepted(&self) -> Result<(), CommandError> {
        self.entry.step_accepted()
    }
    pub(super) fn accept(&self) -> Result<FunctionSnapshot, CommandError> {
        let state = self.function.0.state.lock().unwrap();
        self.entry
            .accept(|| self.function.snapshot_locked(&state), self.missing_write)
    }
    pub(super) fn reject(&self, error: CommandError) {
        self.entry.reject(error)
    }
    pub(super) fn cancel(&self) {
        self.entry.cancel()
    }
    pub(super) fn function(&self) -> &Function {
        &self.function
    }
    pub(super) fn entry(&self) -> &Arc<StepEntry<C>> {
        &self.entry
    }
}

pub(super) struct ThermostatPowerSubmission<P: ThermostatProfile> {
    function: Function,
    entry: Arc<StepEntry<P::Command>>,
}
impl<P: ThermostatProfile> ThermostatPowerSubmission<P> {
    pub(super) fn new(function: Function, entry: Arc<StepEntry<P::Command>>) -> Self {
        Self { function, entry }
    }
}
impl<P: ThermostatProfile> Drop for ThermostatPowerSubmission<P> {
    fn drop(&mut self) {
        let mut state = self.function.0.state.lock().unwrap();
        P::runtime_mut(&mut state)
            .expect("thermostat function has a runtime")
            .commands
            .cancel_pending(&self.entry);
    }
}
