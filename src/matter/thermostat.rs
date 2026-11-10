//! Shared Matter thermostat projection for air conditioners and heaters.

use crate::{
    device::{
        AirConditionerCommand, AirConditionerMode, CommandError, Function, FunctionKind,
        HeaterCommand, HeaterMode, TemperatureGrid, TemperatureSourceState,
    },
    matter::units,
};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::thermostat as th,
    },
    error::{Error, ErrorCode},
    tlv::{Nullable, TLVBuilderParent},
    with,
};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ThermalProjection {
    power: Option<bool>,
    mode: Option<th::SystemModeEnum>,
    temperature: Option<i16>,
    ambient_temperature: Option<i16>,
}
impl ThermalProjection {
    fn read(function: &Function) -> Self {
        let snapshot = function.snapshot();
        if let Some(heater) = snapshot.heater() {
            let ambient_temperature = match heater.temperature_source {
                TemperatureSourceState::Available(reading) => units::temperature_matter_value(
                    reading
                        .value
                        .expect("available built-in temperature has a value"),
                ),
                _ => None,
            };
            return Self {
                power: heater.power.value,
                mode: heater.power.value.map(|_| th::SystemModeEnum::Heat),
                temperature: heater.temperature.value,
                ambient_temperature,
            };
        }
        let air_conditioner = snapshot
            .air_conditioner()
            .expect("thermostat projection requires a thermostat function");
        let ambient_temperature = match snapshot.ambient_temperature().unwrap() {
            crate::device::AmbientTemperature::Available(reading) => {
                units::temperature_matter_value(
                    reading
                        .value
                        .expect("available ambient temperature has a value"),
                )
            }
            _ => None,
        };
        Self {
            power: air_conditioner.power.value,
            mode: air_conditioner.mode.value.map(|mode| match mode {
                AirConditionerMode::Cool => th::SystemModeEnum::Cool,
                AirConditionerMode::Heat => th::SystemModeEnum::Heat,
                AirConditionerMode::Auto => th::SystemModeEnum::Auto,
                AirConditionerMode::Fan => th::SystemModeEnum::FanOnly,
                AirConditionerMode::Dry => th::SystemModeEnum::Dry,
            }),
            temperature: air_conditioner.temperature.value,
            ambient_temperature,
        }
    }
    fn system_mode(self) -> Option<th::SystemModeEnum> {
        match self.power? {
            false => Some(th::SystemModeEnum::Off),
            true => self.mode,
        }
    }
}

pub(crate) trait Profile: 'static {
    const CLUSTER: Cluster<'static>;
    const CONTROL_SEQUENCE: th::ControlSequenceOfOperationEnum;
    const WRITABLE_CONTROL_SEQUENCE: bool;
    const SUPPORTS_COOLING: bool;
    const SETPOINTS: &'static [u32];
}

pub(crate) struct AirConditionerProfile;
impl Profile for AirConditionerProfile {
    const CLUSTER: Cluster<'static> = th::FULL_CLUSTER
        .with_revision(11)
        .with_features(
            th::Feature::HEATING.bits()
                | th::Feature::COOLING.bits()
                | th::Feature::AUTO_MODE.bits(),
        )
        .with_attrs(|a, _, _| matches!(a.id, 0 | 3..=6 | 17 | 18 | 25 | 27 | 28 | 0xfff8..=0xfffd))
        .with_cmds(with!(th::CommandId::SetpointRaiseLower))
        .with_events(with!());
    const CONTROL_SEQUENCE: th::ControlSequenceOfOperationEnum =
        th::ControlSequenceOfOperationEnum::CoolingAndHeating;
    const WRITABLE_CONTROL_SEQUENCE: bool = true;
    const SUPPORTS_COOLING: bool = true;
    const SETPOINTS: &'static [u32] = &[17, 18];
}

pub(crate) struct HeaterProfile;
impl Profile for HeaterProfile {
    const CLUSTER: Cluster<'static> = th::FULL_CLUSTER
        .with_revision(11)
        .with_features(th::Feature::HEATING.bits())
        .with_attrs(|a, _, _| matches!(a.id, 0 | 3 | 4 | 18 | 27 | 28 | 0xfff8..=0xfffd))
        .with_cmds(with!(th::CommandId::SetpointRaiseLower))
        .with_events(with!());
    const CONTROL_SEQUENCE: th::ControlSequenceOfOperationEnum =
        th::ControlSequenceOfOperationEnum::HeatingOnly;
    const WRITABLE_CONTROL_SEQUENCE: bool = false;
    const SUPPORTS_COOLING: bool = false;
    const SETPOINTS: &'static [u32] = &[18];
}

fn command_error(error: CommandError) -> Error {
    log::warn!("Matter thermostat command failed: {error}");
    match error {
        CommandError::Constraint => ErrorCode::ConstraintError,
        CommandError::InvalidInState => ErrorCode::InvalidInState,
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        _ => ErrorCode::Failure,
    }
    .into()
}

pub(in crate::matter) struct Thermostat<P> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<ThermalProjection>,
    reported: Cell<ThermalProjection>,
    profile: std::marker::PhantomData<P>,
}
impl<P: Profile> Thermostat<P> {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let value = ThermalProjection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(value),
            reported: Cell::new(value),
            profile: std::marker::PhantomData,
        }
    }
    fn project(&self) -> ThermalProjection {
        let value = ThermalProjection::read(&self.function);
        let old = self.versioned.replace(value);
        if old.system_mode() != value.system_mode()
            || old.temperature != value.temperature
            || old.ambient_temperature != value.ambient_temperature
        {
            self.dataver.changed();
        }
        value
    }
    pub(super) fn sync(&self) {
        let _ = self.project();
    }
    fn grid(&self) -> TemperatureGrid {
        match self.function.description().kind {
            FunctionKind::AirConditioner { temperature, .. }
            | FunctionKind::Heater { temperature } => temperature,
            _ => unreachable!("thermostat handler has a thermostat function"),
        }
    }
    async fn air_conditioner(&self, command: AirConditionerCommand) -> Result<(), Error> {
        self.function
            .begin_air_conditioner(command)
            .map_err(command_error)?
            .accepted()
            .await
            .map_err(command_error)?;
        Ok(())
    }
    async fn heater(&self, command: HeaterCommand) -> Result<(), Error> {
        self.function
            .begin_heater(command)
            .map_err(command_error)?
            .accepted()
            .await
            .map_err(command_error)?;
        Ok(())
    }
    async fn temperature(&self, value: i16) -> Result<(), Error> {
        if value % 100 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        let temperature = value / 100;
        match self.function.description().kind {
            FunctionKind::AirConditioner { .. } => {
                self.air_conditioner(AirConditionerCommand::Temperature(temperature))
                    .await
            }
            FunctionKind::Heater { .. } => {
                self.heater(HeaterCommand::Temperature(temperature)).await
            }
            _ => unreachable!("thermostat handler has a thermostat function"),
        }
    }
    async fn mode(&self, value: th::SystemModeEnum) -> Result<(), Error> {
        match self.function.description().kind {
            FunctionKind::AirConditioner { .. } => {
                let command = match value {
                    th::SystemModeEnum::Off => AirConditionerCommand::Power(false),
                    th::SystemModeEnum::Cool => {
                        AirConditionerCommand::Mode(AirConditionerMode::Cool)
                    }
                    th::SystemModeEnum::Heat => {
                        AirConditionerCommand::Mode(AirConditionerMode::Heat)
                    }
                    th::SystemModeEnum::Auto => {
                        AirConditionerCommand::Mode(AirConditionerMode::Auto)
                    }
                    th::SystemModeEnum::FanOnly => {
                        AirConditionerCommand::Mode(AirConditionerMode::Fan)
                    }
                    th::SystemModeEnum::Dry => AirConditionerCommand::Mode(AirConditionerMode::Dry),
                    _ => return Err(ErrorCode::ConstraintError.into()),
                };
                self.air_conditioner(command).await
            }
            FunctionKind::Heater { .. } => {
                let command = match value {
                    th::SystemModeEnum::Off => HeaterCommand::Power(false),
                    th::SystemModeEnum::Heat => HeaterCommand::Mode(HeaterMode::Heat),
                    _ => return Err(ErrorCode::ConstraintError.into()),
                };
                self.heater(command).await
            }
            _ => unreachable!("thermostat handler has a thermostat function"),
        }
    }
}

impl<P: Profile> th::ClusterAsyncHandler for Thermostat<P> {
    const CLUSTER: Cluster<'static> = P::CLUSTER;

    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn local_temperature(&self, _: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(self.project().ambient_temperature))
    }
    async fn abs_min_heat_setpoint_limit(&self, _: impl ReadContext) -> Result<i16, Error> {
        Ok(self.grid().min() * 100)
    }
    async fn abs_max_heat_setpoint_limit(&self, _: impl ReadContext) -> Result<i16, Error> {
        Ok(self.grid().effective_max() * 100)
    }
    async fn abs_min_cool_setpoint_limit(&self, _: impl ReadContext) -> Result<i16, Error> {
        Ok(self.grid().min() * 100)
    }
    async fn abs_max_cool_setpoint_limit(&self, _: impl ReadContext) -> Result<i16, Error> {
        Ok(self.grid().effective_max() * 100)
    }
    async fn occupied_cooling_setpoint(&self, _: impl ReadContext) -> Result<i16, Error> {
        self.project()
            .temperature
            .map(|value| value * 100)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn occupied_heating_setpoint(&self, ctx: impl ReadContext) -> Result<i16, Error> {
        self.occupied_cooling_setpoint(ctx).await
    }
    async fn min_setpoint_dead_band(&self, _: impl ReadContext) -> Result<i8, Error> {
        Ok(0)
    }
    async fn control_sequence_of_operation(
        &self,
        _: impl ReadContext,
    ) -> Result<th::ControlSequenceOfOperationEnum, Error> {
        Ok(P::CONTROL_SEQUENCE)
    }
    async fn system_mode(&self, _: impl ReadContext) -> Result<th::SystemModeEnum, Error> {
        self.project()
            .system_mode()
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn set_occupied_cooling_setpoint(
        &self,
        _: impl WriteContext,
        value: i16,
    ) -> Result<(), Error> {
        self.temperature(value).await
    }
    async fn set_occupied_heating_setpoint(
        &self,
        _: impl WriteContext,
        value: i16,
    ) -> Result<(), Error> {
        self.temperature(value).await
    }
    async fn set_min_setpoint_dead_band(&self, _: impl WriteContext, _: i8) -> Result<(), Error> {
        Ok(())
    }
    async fn set_control_sequence_of_operation(
        &self,
        _: impl WriteContext,
        value: th::ControlSequenceOfOperationEnum,
    ) -> Result<(), Error> {
        if P::WRITABLE_CONTROL_SEQUENCE || value == P::CONTROL_SEQUENCE {
            Ok(())
        } else {
            Err(ErrorCode::ConstraintError.into())
        }
    }
    async fn set_system_mode(
        &self,
        _: impl WriteContext,
        value: th::SystemModeEnum,
    ) -> Result<(), Error> {
        self.mode(value).await
    }
    async fn handle_setpoint_raise_lower(
        &self,
        _: impl InvokeContext,
        request: th::SetpointRaiseLowerRequest<'_>,
    ) -> Result<(), Error> {
        let mode = request.mode().map_err(|_| ErrorCode::InvalidCommand)?;
        if matches!(mode, th::SetpointRaiseLowerModeEnum::Cool) && !P::SUPPORTS_COOLING {
            return Err(ErrorCode::InvalidCommand.into());
        }
        let amount = request.amount().map_err(|_| ErrorCode::InvalidCommand)?;
        let base = self
            .project()
            .temperature
            .ok_or(ErrorCode::InvalidInState)?;
        let grid = self.grid();
        let value = (i32::from(base) * 100 + i32::from(amount) * 10).clamp(
            i32::from(grid.min()) * 100,
            i32::from(grid.effective_max()) * 100,
        );
        self.temperature(value as i16).await
    }
    async fn handle_set_weekly_schedule(
        &self,
        _: impl InvokeContext,
        _: th::SetWeeklyScheduleRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_get_weekly_schedule<B: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::GetWeeklyScheduleRequest<'_>,
        _: th::GetWeeklyScheduleResponseBuilder<B>,
    ) -> Result<B, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_clear_weekly_schedule(&self, _: impl InvokeContext) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_set_active_schedule_request(
        &self,
        _: impl InvokeContext,
        _: th::SetActiveScheduleRequestRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_set_active_preset_request(
        &self,
        _: impl InvokeContext,
        _: th::SetActivePresetRequestRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_add_thermostat_suggestion<B: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::AddThermostatSuggestionRequest<'_>,
        _: th::AddThermostatSuggestionResponseBuilder<B>,
    ) -> Result<B, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_remove_thermostat_suggestion(
        &self,
        _: impl InvokeContext,
        _: th::RemoveThermostatSuggestionRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_atomic_request<B: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::AtomicRequestRequest<'_>,
        _: th::AtomicResponseBuilder<B>,
    ) -> Result<B, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let value = self.project();
            let previous = self.reported.replace(value);
            if previous.system_mode() != value.system_mode() {
                ctx.notify_attr_changed(self.endpoint, 0x201, 28);
            }
            if previous.temperature != value.temperature {
                for id in P::SETPOINTS {
                    ctx.notify_attr_changed(self.endpoint, 0x201, *id);
                }
            }
            if previous.ambient_temperature != value.ambient_temperature {
                ctx.notify_attr_changed(self.endpoint, 0x201, 0);
            }
            changes.changed().await;
        }
    }
}
