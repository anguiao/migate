//! Shared Fan Control surface and request checks for fans and air conditioners.

use rs_matter::{
    dm::{
        AsyncHandler, Cluster, HandlerContext, InvokeContext, InvokeReply, MatchContext,
        ReadContext, ReadReply, WriteContext, clusters::decl::fan_control as fc,
    },
    error::{Error, ErrorCode},
    with,
};

pub(super) const fn cluster<const ROCKING: bool>(auto: bool) -> Cluster<'static> {
    fc::FULL_CLUSTER
        .with_revision(6)
        .with_features(
            fc::Feature::MULTI_SPEED.bits()
                | if auto { fc::Feature::AUTO.bits() } else { 0 }
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
        .with_events(with!())
}

pub(super) struct Handler<'a, H>(pub(super) &'a H);

impl<H: fc::ClusterAsyncHandler> AsyncHandler for Handler<'_, H> {
    fn bump_dataver(&self, ctx: impl MatchContext) {
        fc::HandlerAsyncAdaptor(self.0).bump_dataver(ctx);
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if H::CLUSTER.attribute(ctx.attr().attr_id).is_none() {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        fc::HandlerAsyncAdaptor(self.0).read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if H::CLUSTER.attribute(ctx.attr().attr_id).is_none() {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        // Unknown FanMode enum values are constraints, after the version check.
        if ctx.attr().attr_id == 0 && ctx.data().u8().is_ok_and(|value| value > 6) {
            ctx.attr().check_dataver(self.0.dataver())?;
            return Err(ErrorCode::ConstraintError.into());
        }
        fc::HandlerAsyncAdaptor(self.0).write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if H::CLUSTER.command(ctx.cmd().cmd_id).is_none() {
            return Err(ErrorCode::CommandNotFound.into());
        }
        fc::HandlerAsyncAdaptor(self.0).invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        fc::HandlerAsyncAdaptor(self.0).run(ctx).await
    }
}
