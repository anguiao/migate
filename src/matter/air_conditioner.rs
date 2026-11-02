//! Matter projections for a single-target, standard-feedback air conditioner.

use super::on_off::OnOffHandler;
use crate::device::{AirConditionerCommand as Command, CommandError, Function};
use rs_matter::{
    dm::{
        AsyncHandler, ChainedHandler, Dataver, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, ReadContext, ReadReply, WriteContext,
        clusters::{
            app::on_off,
            decl::{fan_control as fc, thermostat as th},
        },
    },
    error::{Error, ErrorCode},
};

mod fan_control;
mod thermostat;

pub(super) use fan_control::AirConditionerFan;
pub(super) use thermostat::Thermostat;

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
