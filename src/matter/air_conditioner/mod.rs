//! Matter projections for a single-target, standard-feedback air conditioner.

use super::{fan_control as fan_cluster, on_off::OnOffHandler};
use crate::device::{AirConditionerCommand as Command, Function};
use rs_matter::{
    dm::{
        AsyncHandler, ChainedHandler, Dataver, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, ReadContext, ReadReply, WriteContext, clusters::app::on_off,
    },
    error::Error,
};

mod fan_control;

pub(super) use fan_control::AirConditionerFan;
pub(super) type Thermostat =
    super::thermostat::Thermostat<super::thermostat::AirConditionerProfile>;

async fn command(function: &Function, value: Command) -> Result<(), Error> {
    let on_error = |error| super::command_error(function, value, error);
    function
        .begin_air_conditioner(value)
        .map_err(on_error)?
        .accepted()
        .await
        .map_err(on_error)?;
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
                &self.thermostat,
                fan_cluster::Handler(&self.fan),
            ),
        )
    }
}
impl<const ROCKING: bool> AsyncHandler for AirConditioner<ROCKING> {
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
