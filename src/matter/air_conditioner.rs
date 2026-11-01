//! Matter projections for a single-target, standard-feedback air conditioner.
use std::cell::Cell;

use super::{on_off::OnOffHandler, units};
use crate::device::{
    AirConditionerCommand as Command, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AmbientTemperature, CommandError, Function, FunctionKind, TemperatureGrid,
};
use rs_matter::{
    dm::{
        AsyncHandler, ChainedHandler, Cluster, Dataver, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::{
            app::on_off,
            decl::{fan_control as fc, thermostat as th},
        },
    },
    error::{Error, ErrorCode},
    tlv::{Nullable, TLVBuilderParent},
    with,
};

fn command_error(error: CommandError) -> Error {
    log::warn!("Matter air-conditioner command failed: {error}");
    match error {
        CommandError::Constraint => ErrorCode::ConstraintError,
        CommandError::InvalidInState => ErrorCode::InvalidInState,
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        _ => ErrorCode::Failure,
    }
    .into()
}
async fn command(function: &Function, value: Command) -> Result<(), Error> {
    function
        .begin_air_conditioner(value)
        .map_err(command_error)?
        .accepted()
        .await
        .map_err(command_error)?;
    Ok(())
}

pub(super) struct AirConditioner<const ROCKING: bool> {
    on_off: OnOffHandler<true>,
    thermostat: Thermostat,
    fan: AirConditionerFan<ROCKING>,
}
impl<const ROCKING: bool> AirConditioner<ROCKING> {
    pub(super) fn new(
        function: Function,
        endpoint: u16,
        on_off: Dataver,
        thermostat: Dataver,
        fan: Dataver,
    ) -> Self {
        Self {
            on_off: OnOffHandler::new(function.clone(), endpoint, on_off),
            thermostat: Thermostat::new(function.clone(), endpoint, thermostat),
            fan: AirConditionerFan::new(function, endpoint, fan),
        }
    }
    pub(super) fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 6) {
            on_off::ClusterAsyncHandler::dataver_changed(&self.on_off);
        }
        if cluster.is_none_or(|id| id == 0x201) {
            self.thermostat.sync();
        }
        if cluster.is_none_or(|id| id == 0x202) {
            self.fan.sync();
        }
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        ChainedHandler::new(
            |_, cluster| cluster == 6,
            on_off::HandlerAsyncAdaptor(&self.on_off),
            ChainedHandler::new(
                |_, cluster| cluster == 0x201,
                th::HandlerAsyncAdaptor(&self.thermostat),
                fc::HandlerAsyncAdaptor(&self.fan),
            ),
        )
    }
}
impl<const ROCKING: bool> AsyncHandler for AirConditioner<ROCKING> {
    fn bump_dataver(&self, _: impl MatchContext) {}
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let cluster = match ctx.attr().cluster_id {
            0x201 => Some(<Thermostat as th::ClusterAsyncHandler>::CLUSTER),
            0x202 => Some(<AirConditionerFan<ROCKING> as fc::ClusterAsyncHandler>::CLUSTER),
            _ => None,
        };
        if cluster.is_some_and(|cluster| cluster.attribute(ctx.attr().attr_id).is_none()) {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let cluster = match ctx.attr().cluster_id {
            0x201 => Some(<Thermostat as th::ClusterAsyncHandler>::CLUSTER),
            0x202 => Some(<AirConditionerFan<ROCKING> as fc::ClusterAsyncHandler>::CLUSTER),
            _ => None,
        };
        if cluster.is_some_and(|cluster| cluster.attribute(ctx.attr().attr_id).is_none()) {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        if ctx.attr().cluster_id == 0x202
            && ctx.attr().attr_id == 0
            && ctx.data().u8().is_ok_and(|value| value > 6)
        {
            ctx.attr()
                .check_dataver(fc::ClusterAsyncHandler::dataver(&self.fan))?;
            return Err(ErrorCode::ConstraintError.into());
        }
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if ctx.cmd().cluster_id == 0x202 || ctx.cmd().cluster_id == 0x201 && ctx.cmd().cmd_id != 0 {
            return Err(ErrorCode::CommandNotFound.into());
        }
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ThermalProjection {
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
pub(super) struct Thermostat {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<ThermalProjection>,
    reported: Cell<ThermalProjection>,
}
impl Thermostat {
    fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let value = ThermalProjection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(value),
            reported: Cell::new(value),
        }
    }
    fn sync(&self) -> ThermalProjection {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FanProjection {
    power: Option<bool>,
    speed: Option<Speed>,
    swing: Option<bool>,
}
impl FanProjection {
    fn read(function: &Function) -> Self {
        let value = function.snapshot().air_conditioner().unwrap();
        Self {
            power: value.power.value,
            speed: value.speed.value,
            swing: value.swing.value,
        }
    }
    fn mode(self) -> Option<fc::FanModeEnum> {
        if !self.power? {
            return Some(fc::FanModeEnum::Off);
        }
        Some(match self.speed? {
            Speed::Auto => fc::FanModeEnum::Auto,
            Speed::Low => fc::FanModeEnum::Low,
            Speed::Medium => fc::FanModeEnum::Medium,
            Speed::High => fc::FanModeEnum::High,
        })
    }
    fn setting(self) -> Result<Option<u8>, Error> {
        if !self.power.ok_or(ErrorCode::Failure)? {
            return Ok(Some(0));
        }
        Ok(match self.speed.ok_or(ErrorCode::Failure)? {
            Speed::Auto => None,
            Speed::Low => Some(1),
            Speed::Medium => Some(2),
            Speed::High => Some(3),
        })
    }
}
pub(super) struct AirConditionerFan<const ROCKING: bool> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<FanProjection>,
    reported: Cell<FanProjection>,
}
impl<const ROCKING: bool> AirConditionerFan<ROCKING> {
    fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let value = FanProjection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(value),
            reported: Cell::new(value),
        }
    }
    fn sync(&self) -> FanProjection {
        let value = FanProjection::read(&self.function);
        let old = self.versioned.replace(value);
        if old.mode() != value.mode()
            || old.setting().ok() != value.setting().ok()
            || ROCKING && old.swing != value.swing
        {
            self.dataver.changed();
        }
        value
    }
    async fn set_speed(&self, value: Nullable<u8>, percent: bool) -> Result<(), Error> {
        let Some(value) = value.into_option() else {
            return Ok(());
        };
        if value > if percent { 100 } else { 3 } {
            return Err(ErrorCode::ConstraintError.into());
        }
        let speed = if percent {
            (u16::from(value) * 3).div_ceil(100) as u8
        } else {
            value
        };
        command(
            &self.function,
            match speed {
                0 => Command::Power(false),
                1 => Command::Speed(Speed::Low),
                2 => Command::Speed(Speed::Medium),
                3 => Command::Speed(Speed::High),
                _ => unreachable!(),
            },
        )
        .await
    }
}
impl<const ROCKING: bool> fc::ClusterAsyncHandler for AirConditionerFan<ROCKING> {
    const CLUSTER: Cluster<'static> = fc::FULL_CLUSTER
        .with_revision(6)
        .with_features(
            fc::Feature::MULTI_SPEED.bits()
                | fc::Feature::AUTO.bits()
                | if ROCKING {
                    fc::Feature::ROCKING.bits()
                } else {
                    0
                },
        )
        .with_attrs(|a, _, _| {
            matches!(a.id, 0..=6 | 0xfff8..=0xfffd) || ROCKING && matches!(a.id, 7 | 8)
        })
        .with_cmds(with!())
        .with_events(with!());
    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn fan_mode(&self, _: impl ReadContext) -> Result<fc::FanModeEnum, Error> {
        self.sync().mode().ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn fan_mode_sequence(
        &self,
        _: impl ReadContext,
    ) -> Result<fc::FanModeSequenceEnum, Error> {
        Ok(fc::FanModeSequenceEnum::OffLowMedHighAuto)
    }
    async fn percent_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(
            self.sync()
                .setting()?
                .map(|speed| (u16::from(speed) * 100 / 3) as u8),
        ))
    }
    async fn percent_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok((u16::from(self.sync().setting()?.ok_or(ErrorCode::Failure)?) * 100 / 3) as u8)
    }
    async fn speed_max(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok(3)
    }
    async fn speed_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.sync().setting()?))
    }
    async fn speed_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        self.sync()
            .setting()?
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn rock_support(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        Ok(fc::RockBitmap::ROCK_UP_DOWN)
    }
    async fn rock_setting(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        match self.sync().swing {
            Some(true) => Ok(fc::RockBitmap::ROCK_UP_DOWN),
            Some(false) => Ok(fc::RockBitmap::empty()),
            None => Err(ErrorCode::Failure.into()),
        }
    }
    async fn set_fan_mode(
        &self,
        _: impl WriteContext,
        value: fc::FanModeEnum,
    ) -> Result<(), Error> {
        command(
            &self.function,
            match value {
                fc::FanModeEnum::Off => Command::Power(false),
                fc::FanModeEnum::Low => Command::Speed(Speed::Low),
                fc::FanModeEnum::Medium => Command::Speed(Speed::Medium),
                fc::FanModeEnum::High | fc::FanModeEnum::On => Command::Speed(Speed::High),
                fc::FanModeEnum::Auto | fc::FanModeEnum::Smart => Command::Speed(Speed::Auto),
            },
        )
        .await
    }
    async fn set_percent_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value, true).await
    }
    async fn set_speed_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value, false).await
    }
    async fn set_rock_setting(
        &self,
        _: impl WriteContext,
        value: fc::RockBitmap,
    ) -> Result<(), Error> {
        match value.bits() {
            0 => command(&self.function, Command::Swing(false)).await,
            2 => command(&self.function, Command::Swing(true)).await,
            _ => Err(ErrorCode::ConstraintError.into()),
        }
    }
    async fn handle_step(
        &self,
        _: impl InvokeContext,
        _: fc::StepRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let value = self.sync();
            let previous = self.reported.replace(value);
            if previous.mode() != value.mode() {
                ctx.notify_attr_changed(self.endpoint, 0x202, 0);
            }
            if previous.setting().ok() != value.setting().ok() {
                for id in [2, 3, 5, 6] {
                    ctx.notify_attr_changed(self.endpoint, 0x202, id);
                }
            }
            if ROCKING && previous.swing != value.swing {
                ctx.notify_attr_changed(self.endpoint, 0x202, 8);
            }
            changes.changed().await;
        }
    }
}
