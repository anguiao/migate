use super::super::{
    LIGHT_ENDPOINT, NODE, basic_info,
    bridged_info::{self, BridgedHandler},
    kv::ProtocolStore,
    light::{LightHandler, LightHooks},
};
use crate::{storage::Store, virtual_device::VirtualLight};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::{Crypto, default_crypto},
    dm::{
        Async, AsyncHandler, AttrChangeNotifier, AttrDetails, CmdDetails, Dataver, EventEmitter,
        EventNumber, HandlerContext, InvokeContext, InvokeReplyInstance, MatchContext, Metadata,
        OperationContext, OwnAttrChangeNotifier, OwnEventEmitter, ReadContext, ReadReplyInstance,
        WriteContext,
        clusters::{
            app::on_off,
            decl::bridged_device_basic_information::{self as bridged, ClusterHandler as _},
            net_comm::NetworksAccess,
        },
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    error::Error,
    im::{
        EthInteractionModelState, ImStats, InteractionModel,
        encoding::{EventPriority, IMBuffer},
        events::EventTLVWrite,
    },
    persist::KvBlobStoreAccess,
    tlv::{TLVElement, TLVTag, TLVWriteParent, Utf8StrBuilder},
    transport::exchange::{Exchange, MatterBuffers},
    utils::storage::{WriteBuf, pooled::Buffers},
};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroU8,
    pin::pin,
};

struct Context<'a, H> {
    base: &'a H,
    command: CmdDetails,
    attribute: AttrDetails,
    data: TLVElement<'a>,
    changes: RefCell<Vec<(u16, u32, u32)>>,
}

impl<'a, H> Context<'a, H> {
    fn new(base: &'a H, cluster_id: u32, attr_id: u32) -> Self {
        Self {
            base,
            command: CmdDetails::new(LIGHT_ENDPOINT, cluster_id, 1, 1, false, None),
            attribute: attr(cluster_id, attr_id),
            data: TLVElement::new(&[0x15, 0x18]),
            changes: RefCell::new(Vec::new()),
        }
    }
}

impl<H: HandlerContext> HandlerContext for Context<'_, H> {
    fn matter(&self) -> &Matter<'_> {
        self.base.matter()
    }
    fn crypto(&self) -> impl Crypto + '_ {
        self.base.crypto()
    }
    fn kv(&self) -> impl KvBlobStoreAccess + '_ {
        self.base.kv()
    }
    fn networks(&self) -> impl NetworksAccess + '_ {
        self.base.networks()
    }
    fn metadata(&self) -> impl Metadata + '_ {
        self.base.metadata()
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        self.base.handler()
    }
    fn buffers(&self) -> impl Buffers<IMBuffer> + '_ {
        self.base.buffers()
    }
    fn im_stats(&self) -> impl ImStats + '_ {
        self.base.im_stats()
    }
    fn notify_fabric_removed(&self, fab_idx: NonZeroU8) {
        self.base.notify_fabric_removed(fab_idx);
    }
}
impl<H: HandlerContext> AttrChangeNotifier for Context<'_, H> {
    fn notify_attr_changed(&self, e: u16, c: u32, a: u32) {
        self.changes.borrow_mut().push((e, c, a));
        self.base.notify_attr_changed(e, c, a);
    }
    fn notify_cluster_changed(&self, _e: u16, _c: u32) {
        panic!("unexpected cluster notification")
    }
    fn notify_endpoint_changed(&self, _e: u16) {
        panic!("unexpected endpoint notification")
    }
    fn notify_all_changed(&self) {
        panic!("unexpected global notification")
    }
}
impl<H: HandlerContext> EventEmitter for Context<'_, H> {
    fn emit_event<F>(
        &self,
        e: u16,
        c: u32,
        event: u32,
        priority: EventPriority,
        f: F,
    ) -> Result<EventNumber, Error>
    where
        F: FnOnce(EventTLVWrite<'_>) -> Result<(), Error>,
    {
        self.base.emit_event(e, c, event, priority, f)
    }
}
impl<H> MatchContext for Context<'_, H> {
    fn endpt(&self) -> Option<u16> {
        Some(LIGHT_ENDPOINT)
    }
    fn cluster(&self) -> Option<u32> {
        Some(self.attribute.cluster_id)
    }
}
impl<H: HandlerContext> OwnAttrChangeNotifier for Context<'_, H> {
    fn notify_own_attr_changed(&self, a: u32) {
        self.notify_attr_changed(LIGHT_ENDPOINT, self.attribute.cluster_id, a);
    }
    fn notify_own_cluster_changed(&self) {
        self.notify_cluster_changed(LIGHT_ENDPOINT, self.attribute.cluster_id);
    }
    fn notify_own_endpoint_changed(&self) {
        self.notify_endpoint_changed(LIGHT_ENDPOINT);
    }
}
impl<H: HandlerContext> OwnEventEmitter for Context<'_, H> {
    fn emit_own_event<F>(
        &self,
        event: u32,
        priority: EventPriority,
        f: F,
    ) -> Result<EventNumber, Error>
    where
        F: FnOnce(EventTLVWrite<'_>) -> Result<(), Error>,
    {
        self.emit_event(
            LIGHT_ENDPOINT,
            self.attribute.cluster_id,
            event,
            priority,
            f,
        )
    }
}
impl<H: HandlerContext> OperationContext for Context<'_, H> {
    fn exchange(&self) -> &Exchange<'_> {
        panic!("these device operations must not require a network exchange")
    }
}
impl<H: HandlerContext> InvokeContext for Context<'_, H> {
    fn cmd(&self) -> &CmdDetails {
        &self.command
    }
    fn data(&self) -> &TLVElement<'_> {
        &self.data
    }
}
impl<H: HandlerContext> ReadContext for Context<'_, H> {
    fn attr(&self) -> &AttrDetails {
        &self.attribute
    }
}
impl<H: HandlerContext> WriteContext for Context<'_, H> {
    fn attr(&self) -> &AttrDetails {
        &self.attribute
    }
    fn data(&self) -> &TLVElement<'_> {
        &self.data
    }
}
fn attr(cluster_id: u32, attr_id: u32) -> AttrDetails {
    AttrDetails {
        endpoint_id: LIGHT_ENDPOINT,
        cluster_id,
        attr_id,
        list_index: None,
        list_chunked: false,
        fab_idx: 1,
        fab_filter: true,
        dataver: None,
        wildcard: false,
        array: false,
        cluster_status: Cell::new(0),
    }
}
#[test]
fn actual_handler_invoke_read_and_report_share_the_device() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.identity().clone();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(ProtocolStore::new(store));
    let light = VirtualLight::new();
    let inner = on_off::OnOffHandler::new_standalone(
        Dataver::new(1),
        LIGHT_ENDPOINT,
        LightHooks::new(&light),
    );
    let handler = LightHandler::new(&inner, &light);
    let im = InteractionModel::new(&matter, &crypto, &buffers, (NODE, &handler), &kv, &state);
    let mut ctx = Context::new(&im, 6, 0);
    block_on(async {
        for (command, power) in [(1, true), (0, false), (2, true), (2, false)] {
            ctx.command.cmd_id = command;
            let mut out = [0; 128];
            handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut out)),
                )
                .await
                .unwrap();
            assert_eq!(
                crate::terminal::handle_line(&light, "status").unwrap(),
                format!("virtual-light-1: {}", if power { "on" } else { "off" })
            );
        }
        ctx.data = TLVElement::new(&[0x15]);
        assert!(
            handler
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(&ctx.command, WriteBuf::new(&mut [0; 128]))
                )
                .await
                .is_err()
        );
        assert!(!light.snapshot().power);
        crate::terminal::handle_line(&light, "on");
        let mut out = [0; 128];
        let mut write = WriteBuf::new(&mut out);
        handler
            .read(&ctx, ReadReplyInstance::new(&ctx.attribute, &mut write))
            .await
            .unwrap();
        let root = TLVElement::new(write.as_slice());
        assert!(
            root.structure()
                .unwrap()
                .find_ctx(1)
                .unwrap()
                .structure()
                .unwrap()
                .find_ctx(2)
                .unwrap()
                .bool()
                .unwrap()
        );
        let mut run = pin!(handler.run(&ctx));
        assert!(poll_once(&mut run).await.is_none());
        assert!(ctx.changes.borrow().contains(&(2, 6, 0)));
        assert!(on_off::ClusterAsyncHandler::dataver(&inner) > 1);
        let count = ctx.changes.borrow().len();
        crate::terminal::handle_line(&light, "on");
        assert!(poll_once(&mut run).await.is_none());
        assert_eq!(ctx.changes.borrow().len(), count);
    });
}

#[test]
fn bridged_label_is_persisted_and_invalid_writes_preserve_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.identity().clone();
    let info = basic_info(&identity);
    let label = bridged_info::load_label(&store).unwrap();
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let kv = matter.kv(ProtocolStore::new(store));
    let bridged = BridgedHandler::new(Dataver::new(1), &identity.light_id, label);
    let handler = Async(bridged::HandlerAdaptor(&bridged));
    let im = InteractionModel::new(&matter, &crypto, &buffers, (NODE, &handler), &kv, &state);
    let ctx = Context::new(
        &im,
        BridgedHandler::CLUSTER.id,
        bridged::AttributeId::NodeLabel as _,
    );
    bridged.set_node_label(&ctx, "书房灯").unwrap();
    assert_eq!(
        bridged_info::load_label(&Store::open(dir.path()).unwrap()).unwrap(),
        "书房灯"
    );
    let count = ctx.changes.borrow().len();
    bridged.set_node_label(&ctx, "书房灯").unwrap();
    assert_eq!(ctx.changes.borrow().len(), count);
    assert!(bridged.set_node_label(&ctx, &"x".repeat(33)).is_err());
    let mut bytes = [0; 64];
    let mut writer = WriteBuf::new(&mut bytes);
    bridged
        .node_label(
            &ctx,
            Utf8StrBuilder::new(TLVWriteParent::new((), &mut writer), &TLVTag::Anonymous),
        )
        .unwrap();
    assert_eq!(TLVElement::new(writer.as_slice()).utf8().unwrap(), "书房灯");
}
