//! Matter fan projection from independent native observations.
use super::{fan_control, on_off::OnOffHandler};
use crate::device::{FanCommand, Function, FunctionKind};
use rs_matter::{
    dm::{
        AsyncHandler, ChainedHandler, Cluster, Dataver, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::{app::on_off, decl::fan_control as fc},
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Projection {
    speed: Option<u8>,
    swing: Option<bool>,
}
impl Projection {
    fn read(function: &Function) -> Self {
        let fan = function.snapshot().fan().expect("fan function");
        Self {
            speed: match fan.power.value {
                Some(false) => Some(0),
                Some(true) => fan.speed.value,
                None => None,
            },
            swing: fan.swing.value,
        }
    }
    fn mode(self, count: u8) -> Option<fc::FanModeEnum> {
        self.speed.map(|speed| {
            if speed == 0 {
                fc::FanModeEnum::Off
            } else if count == 2 && speed == 1 || count >= 3 && speed <= count / 3 {
                fc::FanModeEnum::Low
            } else if count >= 3 && u16::from(speed) <= 2 * u16::from(count) / 3 {
                fc::FanModeEnum::Medium
            } else {
                fc::FanModeEnum::High
            }
        })
    }
}

pub(super) struct Fan<const ROCKING: bool> {
    on_off: OnOffHandler,
    control: FanControl<ROCKING>,
}
impl<const ROCKING: bool> Fan<ROCKING> {
    pub(super) fn new(
        function: Function,
        endpoint: u16,
        on_off: Dataver,
        control: Dataver,
    ) -> Self {
        Self {
            on_off: OnOffHandler::new(function.clone(), endpoint, on_off),
            control: FanControl::new(function, endpoint, control),
        }
    }
    pub(super) fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 6) {
            <OnOffHandler as on_off::ClusterAsyncHandler>::dataver_changed(&self.on_off);
        }
        if cluster.is_none_or(|id| id == 0x202) {
            self.control.sync();
        }
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        ChainedHandler::new(
            |_, cluster| cluster == 6,
            on_off::HandlerAsyncAdaptor(&self.on_off),
            fan_control::Handler(&self.control),
        )
    }
}
impl<const ROCKING: bool> AsyncHandler for Fan<ROCKING> {
    fn bump_dataver(&self, _: impl MatchContext) {}
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}

pub(super) struct FanControl<const ROCKING: bool> {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<Projection>,
    reported: Cell<Projection>,
}
impl<const ROCKING: bool> FanControl<ROCKING> {
    fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let projection = Projection::read(&function);
        Self {
            function,
            endpoint,
            dataver,
            versioned: Cell::new(projection),
            reported: Cell::new(projection),
        }
    }
    fn sync(&self) -> Projection {
        let projection = Projection::read(&self.function);
        if self.versioned.replace(projection) != projection {
            self.dataver.changed();
        }
        projection
    }
    fn count(&self) -> u8 {
        let FunctionKind::Fan { speed_count, .. } = self.function.description().kind else {
            unreachable!("fan handler has a fan function");
        };
        speed_count
    }
    fn speed(&self) -> Result<u8, Error> {
        self.sync().speed.ok_or_else(|| ErrorCode::Failure.into())
    }
    fn percent(&self) -> Result<u8, Error> {
        Ok((u16::from(self.speed()?) * 100 / u16::from(self.count())) as u8)
    }
    async fn command(&self, command: FanCommand) -> Result<(), Error> {
        let on_error = |error| super::command_error(&self.function, command, error);
        let action = self.function.begin_fan(command).map_err(on_error)?;
        action.accepted().await.map_err(on_error)?;
        Ok(())
    }
    async fn set_speed(&self, value: Nullable<u8>, percent: bool) -> Result<(), Error> {
        let value = value.into_option().ok_or(ErrorCode::InvalidInState)?;
        let limit = if percent { 100 } else { self.count() };
        if value > limit {
            return Err(ErrorCode::ConstraintError.into());
        }
        let speed = if percent {
            (u16::from(self.count()) * u16::from(value)).div_ceil(100) as u8
        } else {
            value
        };
        self.command(FanCommand::Speed(speed)).await
    }
}
impl<const ROCKING: bool> fc::ClusterAsyncHandler for FanControl<ROCKING> {
    const CLUSTER: Cluster<'static> = fan_control::cluster::<ROCKING>(false);
    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn fan_mode(&self, _: impl ReadContext) -> Result<fc::FanModeEnum, Error> {
        self.sync()
            .mode(self.count())
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn fan_mode_sequence(
        &self,
        _: impl ReadContext,
    ) -> Result<fc::FanModeSequenceEnum, Error> {
        Ok(match self.count() {
            1 => fc::FanModeSequenceEnum::OffHigh,
            2 => fc::FanModeSequenceEnum::OffLowHigh,
            _ => fc::FanModeSequenceEnum::OffLowMedHigh,
        })
    }
    async fn percent_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(Some(self.percent()?)))
    }
    async fn percent_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        self.percent()
    }
    async fn speed_max(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok(self.count())
    }
    async fn speed_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(Some(self.speed()?)))
    }
    async fn speed_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        self.speed()
    }
    async fn rock_support(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        Ok(fc::RockBitmap::ROCK_LEFT_RIGHT)
    }
    async fn rock_setting(&self, _: impl ReadContext) -> Result<fc::RockBitmap, Error> {
        match self.sync().swing {
            Some(true) => Ok(fc::RockBitmap::ROCK_LEFT_RIGHT),
            Some(false) => Ok(fc::RockBitmap::empty()),
            None => Err(ErrorCode::Failure.into()),
        }
    }
    async fn set_fan_mode(
        &self,
        _: impl WriteContext,
        value: fc::FanModeEnum,
    ) -> Result<(), Error> {
        let command = match value {
            fc::FanModeEnum::Off => FanCommand::Power(false),
            fc::FanModeEnum::Low if self.count() >= 2 => FanCommand::Speed(if self.count() == 2 {
                1
            } else {
                self.count() / 3
            }),
            fc::FanModeEnum::Medium if self.count() >= 3 => {
                FanCommand::Speed((u16::from(self.count()) * 2 / 3) as u8)
            }
            fc::FanModeEnum::High | fc::FanModeEnum::On | fc::FanModeEnum::Smart => {
                FanCommand::Speed(self.count())
            }
            fc::FanModeEnum::Low | fc::FanModeEnum::Medium | fc::FanModeEnum::Auto => {
                return Err(ErrorCode::ConstraintError.into());
            }
        };
        self.command(command).await
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
            0 => self.command(FanCommand::Swing(false)).await,
            1 => self.command(FanCommand::Swing(true)).await,
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
            let projection = self.sync();
            let previous = self.reported.replace(projection);
            if previous.mode(self.count()) != projection.mode(self.count()) {
                ctx.notify_attr_changed(self.endpoint, 0x202, 0);
            }
            if previous.speed != projection.speed {
                for id in [2, 3, 5, 6] {
                    ctx.notify_attr_changed(self.endpoint, 0x202, id);
                }
            }
            if ROCKING && previous.swing != projection.swing {
                ctx.notify_attr_changed(self.endpoint, 0x202, 8);
            }
            changes.changed().await;
        }
    }
}
