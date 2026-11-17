use super::{Context, read_bytes, report_dataver};
use crate::device::{CommandReceiver, Function};
use futures_lite::future::poll_once;
use rs_matter::{
    dm::{AsyncHandler, HandlerContext, InvokeReplyInstance, Metadata},
    error::{Error, ErrorCode},
    tlv::TLVElement,
    utils::storage::WriteBuf,
};
use std::{future::Future, pin::pin};

pub(in crate::matter::tests) async fn assert_unpublished_surface(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    cluster: u32,
    attributes: &[u32],
    commands: &[u32],
) {
    let mut ctx = Context::new(base, cluster, 0);
    base.metadata().access(|node| {
        let declared = node
            .endpoint(ctx.attribute.endpoint_id)
            .unwrap()
            .cluster(cluster)
            .unwrap();
        for &attribute in attributes {
            assert!(declared.attribute(attribute).is_none(), "{attribute:#x}");
        }
        for &command in commands {
            assert!(declared.command(command).is_none(), "{command:#x}");
        }
    });
    for &attribute in attributes {
        ctx.attribute.attr_id = attribute;
        assert_eq!(
            read_bytes(handler, &ctx).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound,
            "read {cluster:#x}.{attribute:#x}"
        );
        ctx.data = TLVElement::new(&[0x04, 1]);
        assert_eq!(
            handler.write(&ctx).await.unwrap_err().code(),
            ErrorCode::AttributeNotFound,
            "write {cluster:#x}.{attribute:#x}"
        );
    }
    for &command in commands {
        ctx.command.cmd_id = command;
        assert_eq!(
            handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                )
                .await
                .unwrap_err()
                .code(),
            ErrorCode::CommandNotFound,
            "invoke {cluster:#x}.{command:#x}"
        );
    }
}

pub(in crate::matter::tests) async fn assert_constraint_after_data_version(
    handler: &impl AsyncHandler,
    ctx: &mut Context<'_, impl HandlerContext>,
    version: u32,
) {
    for (dataver, expected) in [
        (None, ErrorCode::ConstraintError),
        (
            Some(version.wrapping_add(1)),
            ErrorCode::DataVersionMismatch,
        ),
        (Some(version), ErrorCode::ConstraintError),
    ] {
        ctx.attribute.dataver = dataver;
        assert_eq!(
            handler.write(&*ctx).await.unwrap_err().code(),
            expected,
            "write {:#x}.{:#x}, data version {dataver:?}",
            ctx.attribute.cluster_id,
            ctx.attribute.attr_id
        );
    }
}

pub(in crate::matter::tests) async fn assert_fan_mode_constraints(
    handler: &impl AsyncHandler,
    base: &impl HandlerContext,
    receiver: &mut CommandReceiver,
    function: &Function,
    modes: &[u8],
) {
    let before = function.snapshot();
    let version = report_dataver(
        &read_bytes(handler, &Context::new(base, 0x202, 4))
            .await
            .unwrap(),
    );
    for &mode in modes {
        let bytes = [0x04, mode];
        let mut ctx = Context::new(base, 0x202, 0);
        ctx.data = TLVElement::new(&bytes);
        assert_constraint_after_data_version(handler, &mut ctx, version).await;
        assert!(poll_once(receiver.next_operation()).await.is_none());
    }
    assert_eq!(function.snapshot(), before);
}

pub(in crate::matter::tests) async fn assert_receipt_error(
    operation: impl Future<Output = Result<(), Error>>,
    receipt: impl Future<Output = ()>,
    expected: ErrorCode,
) {
    let mut operation = pin!(operation);
    assert!(
        poll_once(operation.as_mut()).await.is_none(),
        "the handler must wait for the device receipt"
    );
    receipt.await;
    assert_eq!(operation.await.unwrap_err().code(), expected);
}
