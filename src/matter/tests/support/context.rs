use rs_matter::{
    Matter,
    crypto::Crypto,
    dm::{
        AsyncHandler, AttrChangeNotifier, AttrDetails, CmdDetails, EventEmitter, EventNumber,
        HandlerContext, InvokeContext, MatchContext, Metadata, OperationContext,
        OwnAttrChangeNotifier, OwnEventEmitter, ReadContext, WriteContext,
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
    utils::storage::pooled::Buffers,
};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroU8,
};

const DEVICE_ENDPOINT: u16 = 2;

pub(in crate::matter) struct Context<'a, H> {
    pub(in crate::matter) base: &'a H,
    pub(in crate::matter) command: CmdDetails,
    pub(in crate::matter) attribute: AttrDetails,
    pub(in crate::matter) data: TLVElement<'a>,
    pub(in crate::matter) changes: RefCell<Vec<(u16, u32, u32)>>,
    pub(in crate::matter) events: RefCell<Vec<(u16, u32, u32)>>,
}

impl<'a, H> Context<'a, H> {
    pub(in crate::matter) fn new(base: &'a H, cluster_id: u32, attr_id: u32) -> Self {
        Self {
            base,
            command: CmdDetails::new(DEVICE_ENDPOINT, cluster_id, 1, 1, false, None),
            attribute: attr(cluster_id, attr_id),
            data: TLVElement::new(&[0x15, 0x18]),
            changes: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
        }
    }
}

impl<'a, H> Context<'a, H> {
    pub(in crate::matter) fn at(base: &'a H, endpoint: u16, cluster: u32, attribute: u32) -> Self {
        let mut context = Self::new(base, cluster, attribute);
        context.attribute.endpoint_id = endpoint;
        context.command.endpoint_id = endpoint;
        context
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
    fn notify_cluster_changed(&self, e: u16, c: u32) {
        self.changes.borrow_mut().push((e, c, u32::MAX));
        self.base.notify_cluster_changed(e, c);
    }
    fn notify_endpoint_changed(&self, e: u16) {
        self.changes.borrow_mut().push((e, u32::MAX, u32::MAX));
        self.base.notify_endpoint_changed(e);
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
        let number = self.base.emit_event(e, c, event, priority, f)?;
        self.events.borrow_mut().push((e, c, event));
        Ok(number)
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
pub(in crate::matter::tests) fn attr(cluster_id: u32, attr_id: u32) -> AttrDetails {
    AttrDetails {
        endpoint_id: DEVICE_ENDPOINT,
        cluster_id,
        attr_id,
        list_index: None,
        list_chunked: false,
        fab_idx: 1,
        fab_filter: true,
        dataver: None,
        wildcard: false,
        array: false,
        cluster_status: Cell::new(None),
    }
}
