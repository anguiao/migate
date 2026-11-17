use super::{ColorHandler, LevelHandler, Lighting, OnOffHandler};
use rs_matter::{
    dm::{
        AsyncHandler, Cluster, HandlerContext, InvokeContext, InvokeReply, MatchContext,
        ReadContext, ReadReply, WriteContext,
        clusters::{
            app::on_off::{self, ClusterAsyncHandler as _},
            decl::{color_control, level_control},
            identify::{self, ClusterHandler as _},
        },
    },
    error::{Error, ErrorCode},
};

pub(in crate::matter) const IDENTIFY: rs_matter::dm::Cluster<'static> =
    identify::IdentifyHandler::<()>::CLUSTER
        .with_cmds(rs_matter::with!(identify::CommandId::Identify));

pub(in crate::matter) struct Dispatch<'a>(pub(in crate::matter) &'a Lighting);

enum SelectedHandler<'a> {
    OnOff(on_off::HandlerAsyncAdaptor<OnOffHandler<'a>>),
    Level(level_control::HandlerAsyncAdaptor<LevelHandler<'a>>),
    ColorTemperature(color_control::HandlerAsyncAdaptor<ColorHandler<'a, false>>),
    ExtendedColor(color_control::HandlerAsyncAdaptor<ColorHandler<'a, true>>),
}

macro_rules! dispatch {
    ($handler:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match $handler {
            SelectedHandler::OnOff(handler) => handler.$method($($arg),*).await,
            SelectedHandler::Level(handler) => handler.$method($($arg),*).await,
            SelectedHandler::ColorTemperature(handler) => handler.$method($($arg),*).await,
            SelectedHandler::ExtendedColor(handler) => handler.$method($($arg),*).await,
        }
    };
}

impl Dispatch<'_> {
    fn select(&self, id: u32) -> Option<(Cluster<'static>, SelectedHandler<'_>)> {
        match id {
            6 => Some((
                OnOffHandler::CLUSTER,
                SelectedHandler::OnOff(on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))),
            )),
            8 if self.0.level() => Some((
                <LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
                SelectedHandler::Level(level_control::HandlerAsyncAdaptor(LevelHandler(self.0))),
            )),
            0x300 if self.0.rgb() => {
                Some((
                    <ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER,
                    SelectedHandler::ExtendedColor(color_control::HandlerAsyncAdaptor(
                        ColorHandler::<true>(self.0),
                    )),
                ))
            }
            0x300 if self.0.ct() => Some((
                <ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER,
                SelectedHandler::ColorTemperature(color_control::HandlerAsyncAdaptor(
                    ColorHandler::<false>(self.0),
                )),
            )),
            _ => None,
        }
    }
}
impl AsyncHandler for Dispatch<'_> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let (cluster, handler) = self
            .select(ctx.attr().cluster_id)
            .ok_or(ErrorCode::AttributeNotFound)?;
        if cluster.attribute(ctx.attr().attr_id).is_none() {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        dispatch!(handler, read(ctx, reply))
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let (cluster, handler) = self
            .select(ctx.attr().cluster_id)
            .ok_or(ErrorCode::UnsupportedAccess)?;
        if cluster.attribute(ctx.attr().attr_id).is_none() {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        dispatch!(handler, write(ctx))
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let (cluster, handler) = self
            .select(ctx.cmd().cluster_id)
            .ok_or(ErrorCode::CommandNotFound)?;
        if cluster.command(ctx.cmd().cmd_id).is_none() {
            return Err(ErrorCode::CommandNotFound.into());
        }
        dispatch!(handler, invoke(ctx, reply))
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))
            .run(ctx)
            .await
    }
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
}
