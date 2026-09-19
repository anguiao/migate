use rs_matter::{
    Matter,
    crypto::Crypto,
    dm::{
        AsyncHandler, AttrChangeNotifier, AttrDetails, CmdDetails, EventEmitter, EventNumber,
        HandlerContext, InvokeContext, MatchContext, Metadata, OperationContext,
        OwnAttrChangeNotifier, OwnEventEmitter, ReadContext, ReadReplyInstance, WriteContext,
        clusters::net_comm::NetworksAccess,
    },
    error::Error,
    im::{
        ImStats,
        encoding::{EventPriority, IMBuffer},
        events::EventTLVWrite,
    },
    persist::KvBlobStoreAccess,
    tlv::TLVElement,
    transport::exchange::Exchange,
    utils::storage::{WriteBuf, pooled::Buffers},
};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroU8,
};

pub(super) struct Context<'a, H> {
    base: &'a H,
    command: CmdDetails,
    attribute: AttrDetails,
    data: TLVElement<'a>,
    changes: RefCell<Vec<(u16, u32, u32)>>,
}

impl<'a, H> Context<'a, H> {
    pub(super) fn has_change(&self, endpoint: u16, cluster: u32, attribute: u32) -> bool {
        self.changes
            .borrow()
            .contains(&(endpoint, cluster, attribute))
    }

    pub(super) fn new_at(base: &'a H, endpoint: u16, cluster_id: u32, attr_id: u32) -> Self {
        Self {
            base,
            command: CmdDetails::new(endpoint, cluster_id, 1, 1, false, None),
            attribute: attr(endpoint, cluster_id, attr_id),
            data: TLVElement::new(&[0x15, 0x18]),
            changes: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn command_at(
        base: &'a H,
        endpoint: u16,
        cluster_id: u32,
        command_id: u32,
        data: &'a [u8],
    ) -> Self {
        let mut context = Self::new_at(base, endpoint, cluster_id, 0);
        context.set_command(command_id, TLVElement::new(data));
        context
    }

    pub(super) fn write_at(
        base: &'a H,
        endpoint: u16,
        cluster_id: u32,
        attribute_id: u32,
        data: &'a [u8],
    ) -> Self {
        let mut context = Self::new_at(base, endpoint, cluster_id, attribute_id);
        context.data = TLVElement::new(data);
        context
    }

    pub(super) fn set_command(&mut self, command_id: u32, data: TLVElement<'a>) {
        self.command.cmd_id = command_id;
        self.data = data;
    }

    pub(super) async fn read_tlv(&self, handler: &impl AsyncHandler) -> Vec<u8>
    where
        H: HandlerContext,
    {
        self.read_tlv_result(handler).await.unwrap()
    }

    pub(super) async fn read_tlv_result(
        &self,
        handler: &impl AsyncHandler,
    ) -> Result<Vec<u8>, Error>
    where
        H: HandlerContext,
    {
        let mut out = vec![0; 512];
        let mut write = WriteBuf::new(&mut out);
        handler
            .read(self, ReadReplyInstance::new(&self.attribute, &mut write))
            .await?;
        Ok(write.as_slice().to_vec())
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
        Some(self.attribute.endpoint_id)
    }
    fn cluster(&self) -> Option<u32> {
        Some(self.attribute.cluster_id)
    }
}
impl<H: HandlerContext> OwnAttrChangeNotifier for Context<'_, H> {
    fn notify_own_attr_changed(&self, a: u32) {
        self.notify_attr_changed(self.attribute.endpoint_id, self.attribute.cluster_id, a);
    }
    fn notify_own_cluster_changed(&self) {
        self.notify_cluster_changed(self.attribute.endpoint_id, self.attribute.cluster_id);
    }
    fn notify_own_endpoint_changed(&self) {
        self.notify_endpoint_changed(self.attribute.endpoint_id);
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
            self.attribute.endpoint_id,
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
fn attr(endpoint_id: u16, cluster_id: u32, attr_id: u32) -> AttrDetails {
    AttrDetails {
        endpoint_id,
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
