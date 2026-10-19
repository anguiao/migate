use super::{ColorHandler, LevelHandler, Lighting, OnOffHandler};
use rs_matter::{
    dm::{
        AsyncHandler, HandlerContext, InvokeContext, InvokeReply, MatchContext, ReadContext,
        ReadReply, WriteContext,
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
impl AsyncHandler for Dispatch<'_> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))
                    .read(ctx, reply)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(LevelHandler(self.0))
                    .read(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<true>(self.0))
                    .read(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<false>(self.0))
                    .read(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))
                    .write(ctx)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(LevelHandler(self.0))
                    .write(ctx)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<true>(self.0))
                    .write(ctx)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<false>(self.0))
                    .write(ctx)
                    .await
            }
            _ => Err(ErrorCode::UnsupportedAccess.into()),
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let cluster = match ctx.cmd().cluster_id {
            6 => &OnOffHandler::CLUSTER,
            8 if self.0.level() => &<LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
            0x300 if self.0.rgb() => {
                &<ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER
            }
            0x300 if self.0.ct() => {
                &<ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER
            }
            _ => return Err(ErrorCode::CommandNotFound.into()),
        };
        if cluster.command(ctx.cmd().cmd_id).is_none() {
            return Err(ErrorCode::CommandNotFound.into());
        }
        match ctx.cmd().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(LevelHandler(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<true>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(ColorHandler::<false>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(OnOffHandler(self.0))
            .run(ctx)
            .await
    }
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
}
