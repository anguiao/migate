use super::storage::{EndpointSceneStore, StoreAdapter};
use rs_matter::{
    Matter,
    crypto::Crypto,
    dm::{
        AsyncHandler, AttrChangeNotifier, AttrDetails, CmdDetails, EventEmitter, EventNumber,
        HandlerContext, InvokeContext, MatchContext, Metadata, OperationContext,
        OwnAttrChangeNotifier, OwnEventEmitter, ReadContext, ReadReply, Reply, WriteContext,
    },
    error::Error,
    im::{
        ImStats,
        encoding::{EventPriority, IMBuffer},
        events::EventTLVWrite,
    },
    persist::KvBlobStoreAccess,
    tlv::{TLVControl, TLVElement, TLVTagType, TLVValueType, TLVWrite, TagType, ToTLV},
    transport::exchange::Exchange,
    utils::storage::pooled::Buffers,
};
use std::num::NonZeroU8;

pub(super) struct SceneContext<'a, C> {
    base: &'a C,
    store: EndpointSceneStore,
}

impl<'a, C> SceneContext<'a, C> {
    pub(super) fn new(base: &'a C, store: StoreAdapter, endpoint: u16) -> Self {
        Self {
            base,
            store: EndpointSceneStore::new(store, endpoint),
        }
    }
}

impl<C: HandlerContext> HandlerContext for SceneContext<'_, C> {
    fn matter(&self) -> &Matter<'_> {
        self.base.matter()
    }
    fn crypto(&self) -> impl Crypto + '_ {
        self.base.crypto()
    }
    fn kv(&self) -> impl KvBlobStoreAccess + '_ {
        self.base.matter().kv(self.store.clone())
    }
    fn networks(&self) -> impl rs_matter::dm::clusters::net_comm::NetworksAccess + '_ {
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

impl<C: AttrChangeNotifier> AttrChangeNotifier for SceneContext<'_, C> {
    fn notify_attr_changed(&self, endpoint: u16, cluster: u32, attribute: u32) {
        self.base.notify_attr_changed(endpoint, cluster, attribute);
    }
    fn notify_cluster_changed(&self, endpoint: u16, cluster: u32) {
        self.base.notify_cluster_changed(endpoint, cluster);
    }
    fn notify_endpoint_changed(&self, endpoint: u16) {
        self.base.notify_endpoint_changed(endpoint);
    }
    fn notify_all_changed(&self) {
        self.base.notify_all_changed();
    }
}

impl<C: EventEmitter> EventEmitter for SceneContext<'_, C> {
    fn emit_event<F>(
        &self,
        endpoint: u16,
        cluster: u32,
        event: u32,
        priority: EventPriority,
        f: F,
    ) -> Result<EventNumber, Error>
    where
        F: FnOnce(EventTLVWrite<'_>) -> Result<(), Error>,
    {
        self.base.emit_event(endpoint, cluster, event, priority, f)
    }
}

impl<C: MatchContext> MatchContext for SceneContext<'_, C> {
    fn endpt(&self) -> Option<u16> {
        self.base.endpt()
    }
    fn cluster(&self) -> Option<u32> {
        self.base.cluster()
    }
}

impl<C: OwnAttrChangeNotifier> OwnAttrChangeNotifier for SceneContext<'_, C> {
    fn notify_own_attr_changed(&self, attribute: u32) {
        self.base.notify_own_attr_changed(attribute);
    }
    fn notify_own_cluster_changed(&self) {
        self.base.notify_own_cluster_changed();
    }
    fn notify_own_endpoint_changed(&self) {
        self.base.notify_own_endpoint_changed();
    }
}

impl<C: OwnEventEmitter> OwnEventEmitter for SceneContext<'_, C> {
    fn emit_own_event<F>(
        &self,
        event: u32,
        priority: EventPriority,
        f: F,
    ) -> Result<EventNumber, Error>
    where
        F: FnOnce(EventTLVWrite<'_>) -> Result<(), Error>,
    {
        self.base.emit_own_event(event, priority, f)
    }
}

impl<C: OperationContext> OperationContext for SceneContext<'_, C> {
    fn exchange(&self) -> &Exchange<'_> {
        self.base.exchange()
    }
    fn accessor(&self) -> Result<rs_matter::acl::Accessor<'_>, Error> {
        self.base.accessor()
    }
}

impl<C: ReadContext> ReadContext for SceneContext<'_, C> {
    fn attr(&self) -> &AttrDetails {
        self.base.attr()
    }
}

impl<C: WriteContext> WriteContext for SceneContext<'_, C> {
    fn attr(&self) -> &AttrDetails {
        self.base.attr()
    }
    fn data(&self) -> &TLVElement<'_> {
        self.base.data()
    }
}

impl<C: InvokeContext> InvokeContext for SceneContext<'_, C> {
    fn cmd(&self) -> &CmdDetails {
        self.base.cmd()
    }
    fn data(&self) -> &TLVElement<'_> {
        self.base.data()
    }
}

pub(super) struct SceneValidReadReply<R>(pub(super) R);

impl<R: ReadReply> ReadReply for SceneValidReadReply<R> {
    fn with_dataver(self, dataver: u32) -> Result<Option<impl Reply>, Error> {
        Ok(self.0.with_dataver(dataver)?.map(|inner| SceneValidReply {
            inner,
            encoded: Vec::new(),
        }))
    }
}

struct SceneValidReply<R> {
    inner: R,
    encoded: Vec<u8>,
}

impl<R: Reply> Reply for SceneValidReply<R> {
    const TAG: TagType = R::TAG;

    fn set<T: ToTLV>(mut self, value: T) -> Result<(), Error> {
        value.to_tlv(&Self::TAG, SceneBufferWriter(&mut self.encoded))?;
        self.complete()
    }

    fn reset(&mut self) {
        self.encoded.clear();
        self.inner.reset();
    }

    fn writer(&mut self) -> impl TLVWrite + Send + '_ {
        SceneBufferWriter(&mut self.encoded)
    }

    fn complete(mut self) -> Result<(), Error> {
        force_scene_valid_false(&mut self.encoded)?;
        {
            let mut writer = self.inner.writer();
            writer.write_raw_data(self.encoded.iter().copied())?;
        }
        self.inner.complete()
    }
}

fn force_scene_valid_false(encoded: &mut [u8]) -> Result<(), Error> {
    let base = encoded.as_ptr() as usize;
    let root = TLVElement::new(encoded);
    let mut offsets = Vec::new();
    for entry in root.array()?.iter() {
        let scene_valid = entry?.structure()?.ctx(3)?;
        scene_valid.bool()?;
        offsets.push(scene_valid.raw_data().as_ptr() as usize - base);
    }
    for offset in offsets {
        encoded[offset] = TLVControl::new(TLVTagType::Context, TLVValueType::False).as_raw();
    }
    Ok(())
}

struct SceneBufferWriter<'a>(&'a mut Vec<u8>);

impl TLVWrite for SceneBufferWriter<'_> {
    type Position = usize;

    fn write(&mut self, byte: u8) -> Result<(), Error> {
        self.0.push(byte);
        Ok(())
    }

    fn get_tail(&self) -> Self::Position {
        self.0.len()
    }

    fn rewind_to(&mut self, position: Self::Position) {
        self.0.truncate(position);
    }
}
