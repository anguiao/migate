use super::Context;
use rs_matter::{
    dm::{AsyncHandler, HandlerContext, ReadReplyInstance},
    error::Error,
    tlv::TLVElement,
    utils::storage::WriteBuf,
};

pub(in super::super) async fn read_bytes(
    handler: &impl AsyncHandler,
    ctx: &Context<'_, impl HandlerContext>,
) -> Result<Vec<u8>, Error> {
    let mut buffer = [0; 4096];
    let mut write = WriteBuf::new(&mut buffer);
    handler
        .read(ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
        .await?;
    Ok(write.as_slice().to_vec())
}

pub(in super::super) fn report_value(bytes: &[u8]) -> TLVElement<'_> {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(2)
        .unwrap()
}

pub(in super::super) fn report_dataver(bytes: &[u8]) -> u32 {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(0)
        .unwrap()
        .u32()
        .unwrap()
}
