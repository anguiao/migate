//! Matter Window Covering projection of one native curtain function.
use crate::device::{CommandError, CurtainCommand, CurtainMotion, Function};
use rs_matter::{
    dm::{
        Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, WriteContext,
        clusters::decl::window_covering as wc,
    },
    error::{Error, ErrorCode},
    tlv::Nullable,
    with,
};
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Projection {
    current: Option<u32>,
    target: Option<u32>,
    status: Option<CurtainMotion>,
}
impl Projection {
    fn read(function: &Function) -> Self {
        let (native, target) = function.matter_curtain();
        Self {
            current: native.current_position.value,
            target,
            status: native.status.value,
        }
    }
}

pub(super) struct CurtainHandler {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    versioned: Cell<Projection>,
    reported: Cell<Projection>,
}
impl CurtainHandler {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
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
    async fn command(&self, command: CurtainCommand) -> Result<(), Error> {
        let action: crate::device::CurtainAction = self
            .function
            .begin_curtain(command)
            .map_err(command_error)?;
        action.accepted().await.map_err(command_error)?;
        Ok(())
    }
    async fn no_args(&self, ctx: impl InvokeContext, command: CurtainCommand) -> Result<(), Error> {
        ctx.data()
            .structure()
            .and_then(|_| ctx.data().raw_value())
            .map_err(|_| ErrorCode::InvalidCommand)?;
        self.command(command).await
    }
}
fn command_error(error: CommandError) -> Error {
    log::warn!("Matter curtain command failed: {error}");
    match error {
        CommandError::Unsupported => ErrorCode::CommandNotFound,
        CommandError::Timeout => ErrorCode::RxTimeout,
        _ => ErrorCode::Failure,
    }
    .into()
}
fn percent100ths(value: Option<u32>) -> Nullable<u16> {
    Nullable::new(value.map(|v| ((100 - v) * 100) as u16))
}
impl wc::ClusterAsyncHandler for CurtainHandler {
    const CLUSTER: Cluster<'static> = wc::FULL_CLUSTER
        .with_features(wc::Feature::LIFT.bits() | wc::Feature::POSITION_AWARE_LIFT.bits())
        .with_attrs(|a, _, _| matches!(a.id, 0 | 7 | 8 | 10 | 11 | 13 | 14 | 23 | 0xfff8..=0xfffd))
        .with_cmds(with!(
            wc::CommandId::UpOrOpen
                | wc::CommandId::DownOrClose
                | wc::CommandId::StopMotion
                | wc::CommandId::GoToLiftPercentage
        ))
        .with_events(with!());
    fn dataver(&self) -> u32 {
        self.sync();
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn r#type(&self, _: impl ReadContext) -> Result<wc::Type, Error> {
        Ok(wc::Type::Drapery)
    }
    async fn config_status(&self, _: impl ReadContext) -> Result<wc::ConfigStatus, Error> {
        Ok(wc::ConfigStatus::OPERATIONAL | wc::ConfigStatus::LIFT_POSITION_AWARE)
    }
    async fn current_position_lift_percentage(
        &self,
        _: impl ReadContext,
    ) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::new(self.sync().current.map(|v| (100 - v) as u8)))
    }
    async fn operational_status(
        &self,
        _: impl ReadContext,
    ) -> Result<wc::OperationalStatus, Error> {
        let bits = match self.sync().status {
            Some(CurtainMotion::Stopped) => 0,
            Some(CurtainMotion::Opening) => 5,
            Some(CurtainMotion::Closing) => 10,
            None => return Err(ErrorCode::Failure.into()),
        };
        Ok(wc::OperationalStatus::from_bits_retain(bits))
    }
    async fn target_position_lift_percent_100_ths(
        &self,
        _: impl ReadContext,
    ) -> Result<Nullable<u16>, Error> {
        Ok(percent100ths(self.sync().target))
    }
    async fn end_product_type(&self, _: impl ReadContext) -> Result<wc::EndProductType, Error> {
        Ok(wc::EndProductType::Unknown)
    }
    async fn current_position_lift_percent_100_ths(
        &self,
        _: impl ReadContext,
    ) -> Result<Nullable<u16>, Error> {
        Ok(percent100ths(self.sync().current))
    }
    async fn mode(&self, _: impl ReadContext) -> Result<wc::Mode, Error> {
        Ok(wc::Mode::empty())
    }
    async fn set_mode(&self, _: impl WriteContext, value: wc::Mode) -> Result<(), Error> {
        if !value.is_empty() {
            return Err(ErrorCode::ConstraintError.into());
        }
        Ok(())
    }
    async fn handle_up_or_open(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.no_args(ctx, CurtainCommand::Target(100.0)).await
    }
    async fn handle_down_or_close(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.no_args(ctx, CurtainCommand::Target(0.0)).await
    }
    async fn handle_stop_motion(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.no_args(ctx, CurtainCommand::Stop).await
    }
    async fn handle_go_to_lift_percentage(
        &self,
        ctx: impl InvokeContext,
        request: wc::GoToLiftPercentageRequest<'_>,
    ) -> Result<(), Error> {
        ctx.data()
            .structure()
            .and_then(|_| ctx.data().raw_value())
            .map_err(|_| ErrorCode::InvalidCommand)?;
        let value = request
            .lift_percent_100_ths_value()
            .map_err(|_| ErrorCode::InvalidCommand)?;
        if value > 10000 {
            return Err(ErrorCode::ConstraintError.into());
        }
        self.command(CurtainCommand::Target((10000 - value) as f64 / 100.0))
            .await
    }
    async fn handle_go_to_lift_value(
        &self,
        _: impl InvokeContext,
        _: wc::GoToLiftValueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_go_to_tilt_value(
        &self,
        _: impl InvokeContext,
        _: wc::GoToTiltValueRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_go_to_tilt_percentage(
        &self,
        _: impl InvokeContext,
        _: wc::GoToTiltPercentageRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        loop {
            let projection = self.sync();
            let previous = self.reported.replace(projection);
            if previous.current != projection.current {
                ctx.notify_attr_changed(self.endpoint, 0x102, 8);
                ctx.notify_attr_changed(self.endpoint, 0x102, 14);
            }
            if previous.target != projection.target {
                ctx.notify_attr_changed(self.endpoint, 0x102, 11);
            }
            if previous.status != projection.status {
                ctx.notify_attr_changed(self.endpoint, 0x102, 10);
            }
            changes.changed().await;
        }
    }
}
