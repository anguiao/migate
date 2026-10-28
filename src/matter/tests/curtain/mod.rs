use super::support::{
    Context, attr, model_handler, read_bytes, report_dataver, report_value, single_entry,
};
use crate::{
    device::{
        Availability, CommandError, CurtainCommand, CurtainMotion, CurtainValue, DeviceKey,
        DeviceRequest, FunctionDescription, FunctionKind, PhysicalDevice,
        PhysicalDeviceDescription,
    },
    matter::{model::basic_info, storage::StoreAdapter},
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, HandlerContext, InvokeReplyInstance, Metadata,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    error::ErrorCode,
    im::{InteractionModel, InteractionModelState},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

fn curtain() -> (PhysicalDevice, crate::device::CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "test-curtain"),
            "Curtain",
            vec![FunctionDescription {
                id: "main".into(),
                label: "Curtain".into(),
                kind: FunctionKind::Curtain,
            }],
        )
        .unwrap(),
    );
    (device, receivers.remove(0))
}

macro_rules! setup {
    ($device:ident, $receiver:ident, $handler:ident, $ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let ($device, mut $receiver) = curtain();
        let $handler = model_handler(vec![single_entry(&$device)]);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
        let mut $ctx = Context::new(&im, 0x102, 0);
    };
}

async fn read_u16(handler: &impl AsyncHandler, ctx: &Context<'_, impl HandlerContext>) -> u16 {
    report_value(&read_bytes(handler, ctx).await.unwrap())
        .u16()
        .unwrap()
}

async fn next_curtain(
    receiver: &mut crate::device::CommandReceiver,
) -> crate::device::CurtainRequest {
    let DeviceRequest::Curtain(request) = receiver.next_operation().await.unwrap() else {
        panic!("expected curtain request")
    };
    request
}

mod commands;
mod metadata;
mod reports;
