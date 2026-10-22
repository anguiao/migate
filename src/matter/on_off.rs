use crate::device::{Command, CommandError, Function, Power};
use rs_matter::{
    dm::{Cluster, Dataver, HandlerContext, InvokeContext, ReadContext, clusters::app::on_off},
    error::{Error, ErrorCode},
    with,
};

/// The shared Function is the only power state and command boundary.
pub(super) struct OnOffHandler {
    function: Function,
    endpoint: u16,
    dataver: Dataver,
    reported: std::cell::Cell<Option<bool>>,
}
impl OnOffHandler {
    pub(super) fn new(function: Function, endpoint: u16, dataver: Dataver) -> Self {
        let reported = std::cell::Cell::new(function.snapshot().power().and_then(Power::value));
        Self {
            function,
            endpoint,
            dataver,
            reported,
        }
    }
    async fn execute(&self, ctx: impl InvokeContext, command: Command) -> Result<(), Error> {
        ctx.data()
            .structure()
            .and_then(|_| ctx.data().raw_value())
            .map_err(|_| ErrorCode::InvalidCommand)?;
        self.function.execute(command).await.map_err(|error| {
            log::warn!(
                "Matter {}/{} {command:?} failed: {error}",
                self.function.function_key().device.id,
                self.function.function_key().id,
            );
            match error {
                CommandError::Unsupported => ErrorCode::CommandNotFound,
                CommandError::Timeout => ErrorCode::RxTimeout,
                _ => ErrorCode::Failure,
            }
        })?;
        log::info!(
            "Matter {}/{} {command:?}: accepted",
            self.function.function_key().device.id,
            self.function.function_key().id,
        );
        Ok(())
    }
}
impl on_off::ClusterAsyncHandler for OnOffHandler {
    const CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
        .with_features(0)
        .with_attrs(with!(required))
        .with_cmds(with!(
            on_off::CommandId::Off | on_off::CommandId::On | on_off::CommandId::Toggle
        ));
    fn dataver(&self) -> u32 {
        self.dataver.get()
    }
    fn dataver_changed(&self) {
        self.dataver.changed();
    }
    async fn on_off(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        self.function
            .snapshot()
            .power()
            .and_then(Power::value)
            .ok_or_else(|| ErrorCode::Failure.into())
    }
    async fn handle_off(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.execute(ctx, Command::Off).await
    }
    async fn handle_on(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.execute(ctx, Command::On).await
    }
    async fn handle_toggle(&self, ctx: impl InvokeContext) -> Result<(), Error> {
        self.execute(ctx, Command::Toggle).await
    }
    async fn handle_off_with_effect(
        &self,
        _ctx: impl InvokeContext,
        _request: on_off::OffWithEffectRequest<'_>,
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
        _request: on_off::OnWithTimedOffRequest<'_>,
    ) -> Result<(), Error> {
        Err(ErrorCode::CommandNotFound.into())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.function.subscribe();
        let mut value = self.function.snapshot().power().and_then(Power::value);
        loop {
            if self.reported.replace(value) != value {
                ctx.notify_attr_changed(
                    self.endpoint,
                    Self::CLUSTER.id,
                    on_off::AttributeId::OnOff as _,
                );
            }
            value = changes.changed().await.power().and_then(Power::value);
        }
    }
}
