//! Matter fan projection from independent native observations.
use super::on_off::OnOffHandler;
use crate::device::{CommandError, FanCommand, Function};
use rs_matter::{
    dm::{
        AsyncHandler, ChainedHandler, Cluster, Dataver, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::{app::on_off, decl::fan_control as fc},
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
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
    fn mode(self) -> Option<fc::FanModeEnum> {
        self.speed.map(|speed| match speed {
            0 => fc::FanModeEnum::Off,
            1..=33 => fc::FanModeEnum::Low,
            34..=66 => fc::FanModeEnum::Medium,
            _ => fc::FanModeEnum::High,
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
            fc::HandlerAsyncAdaptor(&self.control),
        )
    }
}
impl<const ROCKING: bool> AsyncHandler for Fan<ROCKING> {
    fn bump_dataver(&self, _: impl MatchContext) {}
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if ctx.attr().cluster_id == 0x202
            && <FanControl<ROCKING> as fc::ClusterAsyncHandler>::CLUSTER
                .attribute(ctx.attr().attr_id)
                .is_none()
        {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if ctx.attr().cluster_id == 0x202 {
            if <FanControl<ROCKING> as fc::ClusterAsyncHandler>::CLUSTER
                .attribute(ctx.attr().attr_id)
                .is_none()
            {
                return Err(ErrorCode::AttributeNotFound.into());
            }
            // The generated enum decoder returns Invalid for unknown values.
            // FanMode reserves those values as a protocol constraint instead.
            if ctx.attr().attr_id == 0 && ctx.data().u8().is_ok_and(|value| value > 6) {
                ctx.attr()
                    .check_dataver(fc::ClusterAsyncHandler::dataver(&self.control))?;
                return Err(ErrorCode::ConstraintError.into());
            }
        }
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if ctx.cmd().cluster_id == 0x202 {
            return Err(ErrorCode::CommandNotFound.into());
        }
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
    fn speed(&self) -> Result<u8, Error> {
        self.sync().speed.ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn command(&self, command: FanCommand) -> Result<(), Error> {
        let action = self.function.begin_fan(command).map_err(command_error)?;
        action.accepted().await.map_err(command_error)?;
        Ok(())
    }
    async fn set_speed(&self, value: Nullable<u8>) -> Result<(), Error> {
        let value = value.into_option().ok_or(ErrorCode::InvalidInState)?;
        if value > 100 {
            return Err(ErrorCode::ConstraintError.into());
        }
        self.command(FanCommand::Speed(value)).await
    }
}
fn command_error(error: CommandError) -> Error {
    log::warn!("Matter fan command failed: {error}");
    match error {
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        _ => ErrorCode::Failure,
    }
    .into()
}
impl<const ROCKING: bool> fc::ClusterAsyncHandler for FanControl<ROCKING> {
    const CLUSTER: Cluster<'static> = fc::FULL_CLUSTER
        .with_revision(6)
        .with_features(
            fc::Feature::MULTI_SPEED.bits()
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
        Ok(fc::FanModeSequenceEnum::OffLowMedHigh)
    }
    async fn percent_setting(&self, _: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(Some(self.speed()?)))
    }
    async fn percent_current(&self, _: impl ReadContext) -> Result<u8, Error> {
        self.speed()
    }
    async fn speed_max(&self, _: impl ReadContext) -> Result<u8, Error> {
        Ok(100)
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
            fc::FanModeEnum::Low => FanCommand::Speed(33),
            fc::FanModeEnum::Medium => FanCommand::Speed(66),
            fc::FanModeEnum::High | fc::FanModeEnum::On | fc::FanModeEnum::Smart => {
                FanCommand::Speed(100)
            }
            fc::FanModeEnum::Auto => return Err(ErrorCode::ConstraintError.into()),
        };
        self.command(command).await
    }
    async fn set_percent_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value).await
    }
    async fn set_speed_setting(
        &self,
        _: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        self.set_speed(value).await
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
            if previous.mode() != projection.mode() {
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
