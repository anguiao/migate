use super::support::{Context, attr, model_handler, read_bytes, report_dataver, report_value};
use crate::{
    device::{Availability, DeviceRequest, LightDimension, LightPatch},
    matter::{
        model::{self, basic_info},
        storage::StoreAdapter,
    },
    storage::Store,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, InvokeReplyInstance, Metadata,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
    },
    error::ErrorCode,
    im::{InteractionModel, InteractionModelState},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
    utils::storage::WriteBuf,
};
use std::pin::pin;

macro_rules! setup_light {
    ($rgb:expr, $device:ident, $receiver:ident, $handler:ident, $ctx:ident) => {
        setup_light!(@source crate::matter::tests::support::light($rgb),$device,$receiver,$handler,$ctx);
    };
    (@source $source:expr, $device:ident, $receiver:ident, $handler:ident, $ctx:ident) => {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let ($device, mut $receiver) = $source;
        let $handler = model_handler(vec![crate::matter::tests::support::single_entry(&$device)]);
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: InteractionModelState = InteractionModelState::new();
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
        let mut $ctx = Context::new(&im, 8, 0);
    };
}

mod commands;
mod lifecycle;
mod versions;
