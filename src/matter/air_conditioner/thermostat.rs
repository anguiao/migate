use super::{Command, command};
use crate::{
    device::{
        AirConditionerMode as Mode, AmbientTemperature, Function, FunctionKind, TemperatureGrid,
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
pub(super) struct ThermalProjection {
    power: Option<bool>,
    mode: Option<Mode>,
    temperature: Option<i16>,
    ambient_temperature: Option<i16>,
}
impl ThermalProjection {
    fn read(function: &Function) -> Self {
        let snapshot = function.snapshot();
        let value = snapshot.air_conditioner().unwrap();
        let ambient_temperature = match snapshot.ambient_temperature().unwrap() {
            AmbientTemperature::Available(reading) => units::temperature_matter_value(
                reading
                    .value
                    .expect("available ambient temperature has a value"),
            ),
            _ => None,
        };
        Self {
            power: value.power.value,
            mode: value.mode.value,
            temperature: value.temperature.value,
            ambient_temperature,
        }
    }
    fn mode(self) -> Option<th::SystemModeEnum> {
        match self.power? {
            false => Some(th::SystemModeEnum::Off),
            true => Some(match self.mode? {
                Mode::Cool => th::SystemModeEnum::Cool,
                Mode::Heat => th::SystemModeEnum::Heat,
                Mode::Auto => th::SystemModeEnum::Auto,
                Mode::Fan => th::SystemModeEnum::FanOnly,
                Mode::Dry => th::SystemModeEnum::Dry,
            }),
        }
    }
}
pub(in crate::matter) struct Thermostat {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<ThermalProjection>,
    reported: Cell<ThermalProjection>,
}
impl Thermostat {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let value = ThermalProjection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(value),
            reported: Cell::new(value),
        }
    }
    pub(super) fn sync(&self) -> ThermalProjection {
        let value = ThermalProjection::read(&self.function);
        let old = self.versioned.replace(value);
        if old.mode() != value.mode()
            || old.temperature != value.temperature
            || old.ambient_temperature != value.ambient_temperature
        {
            self.dataver.changed();
        }
        value
    }
    fn grid(&self) -> TemperatureGrid {
        let FunctionKind::AirConditioner { temperature, .. } = self.function.description().kind
        else {
            unreachable!()
        };
        temperature
    }
    async fn temperature(&self, value: i16) -> Result<(), Error> {
        if value % 100 != 0 {
            return Err(ErrorCode::ConstraintError.into());
        }
        command(&self.function, Command::Temperature(value / 100)).await
    }
}
impl th::ClusterAsyncHandler for Thermostat {
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
    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn local_temperature(&self, _: impl ReadContext) -> Result<Nullable<i16>, Error> {
        Ok(Nullable::new(self.sync().ambient_temperature))
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
        self.sync()
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
        Ok(th::ControlSequenceOfOperationEnum::CoolingAndHeating)
    }
    async fn system_mode(&self, _: impl ReadContext) -> Result<th::SystemModeEnum, Error> {
        self.sync().mode().ok_or_else(|| ErrorCode::Failure.into())
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
        _: th::ControlSequenceOfOperationEnum,
    ) -> Result<(), Error> {
        Ok(())
    }
    async fn set_system_mode(
        &self,
        _: impl WriteContext,
        value: th::SystemModeEnum,
    ) -> Result<(), Error> {
        let operation = match value {
            th::SystemModeEnum::Off => Command::Power(false),
            th::SystemModeEnum::Cool => Command::Mode(Mode::Cool),
            th::SystemModeEnum::Heat => Command::Mode(Mode::Heat),
            th::SystemModeEnum::Auto => Command::Mode(Mode::Auto),
            th::SystemModeEnum::FanOnly => Command::Mode(Mode::Fan),
            th::SystemModeEnum::Dry => Command::Mode(Mode::Dry),
            _ => return Err(ErrorCode::ConstraintError.into()),
        };
        command(&self.function, operation).await
    }
    async fn handle_setpoint_raise_lower(
        &self,
        _: impl InvokeContext,
        request: th::SetpointRaiseLowerRequest<'_>,
    ) -> Result<(), Error> {
        request.mode().map_err(|_| ErrorCode::InvalidCommand)?;
        let amount = request.amount().map_err(|_| ErrorCode::InvalidCommand)?;
        let base = self.sync().temperature.ok_or(ErrorCode::InvalidInState)?;
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
    async fn handle_get_weekly_schedule<P: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::GetWeeklyScheduleRequest<'_>,
        _: th::GetWeeklyScheduleResponseBuilder<P>,
    ) -> Result<P, Error> {
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
    async fn handle_add_thermostat_suggestion<P: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::AddThermostatSuggestionRequest<'_>,
        _: th::AddThermostatSuggestionResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_remove_thermostat_suggestion(
        &self,
        _: impl InvokeContext,
        _: th::RemoveThermostatSuggestionRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_atomic_request<P: TLVBuilderParent>(
        &self,
        _: impl InvokeContext,
        _: th::AtomicRequestRequest<'_>,
        _: th::AtomicResponseBuilder<P>,
    ) -> Result<P, Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let value = self.sync();
            let previous = self.reported.replace(value);
            if previous.mode() != value.mode() {
                ctx.notify_attr_changed(self.endpoint, 0x201, 28);
            }
            if previous.temperature != value.temperature {
                for id in [17, 18] {
                    ctx.notify_attr_changed(self.endpoint, 0x201, id);
                }
            }
            if previous.ambient_temperature != value.ambient_temperature {
                ctx.notify_attr_changed(self.endpoint, 0x201, 0);
            }
            changes.changed().await;
        }
    }
}
