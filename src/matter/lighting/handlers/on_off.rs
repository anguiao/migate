use super::super::Lighting;
use crate::device::Command;
use rs_matter::{
    dm::{Cluster, HandlerContext, InvokeContext, ReadContext, clusters::decl::on_off as o},
    error::{Error, ErrorCode},
    with,
};

pub(in crate::matter) struct OnOffHandler<'a>(pub(in crate::matter) &'a Lighting);

impl o::ClusterAsyncHandler for OnOffHandler<'_> {
    const CLUSTER: Cluster<'static> = o::FULL_CLUSTER
        .with_features(0)
        .with_attrs(|a, _, _| a.id == 0 || a.id >= 0xfff8)
        .with_cmds(with!(
            o::CommandId::Off | o::CommandId::On | o::CommandId::Toggle
        ));
    fn dataver(&self) -> u32 {
        self.0.sync_versions();
        self.0.report.on_off_version.get()
    }
    fn dataver_changed(&self) {
        self.0.report.on_off_version.changed();
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.0.run(ctx).await
    }
    async fn on_off(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.0
            .refresh()
            .power
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn handle_on(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::On).await
    }
    async fn handle_off(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::Off).await
    }
    async fn handle_toggle(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        validate_power(&ctx)?;
        self.0.power(Command::Toggle).await
    }
    async fn handle_off_with_effect(
        &self,
        _ctx: impl InvokeContext,
        _request: o::OffWithEffectRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_on_with_recall_global_scene(
        &self,
        _ctx: impl InvokeContext,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn handle_on_with_timed_off(
        &self,
        _ctx: impl InvokeContext,
        _request: o::OnWithTimedOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
}
fn validate_power(ctx: &impl InvokeContext) -> Result<(), Error> {
    ctx.data()
        .structure()
        .and_then(|_| ctx.data().raw_value())
        .map_err(|_| ErrorCode::InvalidCommand.into())
        .map(|_| ())
}
