use super::{bridged_info::BridgedHandler, on_off::OnOffHandler, storage::StoreAdapter};
use crate::{
    device::{DeviceRegistry, PhysicalDevice},
    storage::{Identity, StorageError, Store, StoredDevice},
};
use rand::RngExt as _;
use rs_matter::{
    Matter, clusters, devices,
    dm::{
        AsyncHandler, Dataver, DeviceType, Endpoint, Handler, HandlerContext, InvokeContext,
        InvokeReply, LifecycleOp, MatchContext, Metadata, Node, ReadContext, ReadReply,
        SemanticTag, WriteContext,
        clusters::{
            app::on_off::{self, ClusterAsyncHandler as _},
            basic_info::BasicInfoConfig,
            decl::bridged_device_basic_information::{self as bridged, ClusterHandler as _},
            desc::{self, ClusterHandler as _},
            groups::{self, ClusterHandler as _},
            identify::{self, ClusterHandler as _},
        },
        devices::{DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, test::TEST_DEV_DET},
        endpoints,
        networks::SysNetifs,
    },
    error::{Error, ErrorCode},
    root_endpoint,
    utils::sync::DynBase,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
    task::Poll,
};

const AGGREGATOR_ENDPOINT: u16 = 1;
const ROOT: Endpoint<'static> = root_endpoint!(eth);
// Basic interoperability intentionally omits the standard's Lighting/Scenes requirements.
const ON_OFF_PLUG: DeviceType = DeviceType {
    dtype: 0x010a,
    drev: 3,
};
const SINGLE_TYPES: &[DeviceType] = devices!(ON_OFF_PLUG, DEV_TYPE_BRIDGED_NODE);
const SINGLE_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    BridgedHandler::CLUSTER,
    OnOffHandler::CLUSTER
);
const PARENT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] =
    clusters!(desc::DescHandler::CLUSTER, BridgedHandler::CLUSTER);
const CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    OnOffHandler::CLUSTER
);

pub(super) fn basic_info(identity: &Identity) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
        product_name: "MiGate",
        device_name: "MiGate",
        product_label: "MiGate",
        serial_no: &identity.bridge_id,
        unique_id: &identity.bridge_id,
        ..TEST_DEV_DET
    }
}
pub(super) fn initialize_basic_info(
    matter: &Matter<'_>,
    kv: impl rs_matter::persist::KvBlobStoreAccess,
    missing: bool,
) -> Result<(), Error> {
    if missing {
        let mut settings = rs_matter::dm::clusters::basic_info::BasicInfoSettings::new();
        settings.node_label.push_str("MiGate").unwrap();
        rs_matter::persist::Persist::new(&kv)
            .store_tlv(rs_matter::persist::BASIC_INFO_KEY, &settings)?;
        matter.startup(&kv)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Versions {
    descriptor: u32,
    groups: u32,
    identify: u32,
    on_off: u32,
    bridged: u32,
}
struct Entry {
    endpoint: u16,
    started: Cell<bool>,
    device: Option<PhysicalDevice>,
    descriptor: Dataver,
    children: Vec<u16>,
    tag: Option<(u8, String)>,
    groups: Dataver,
    identify: Option<identify::IdentifyHandler>,
    on_off: Option<OnOffHandler>,
    bridged: Option<BridgedHandler>,
}
impl Entry {
    fn versions(&self) -> Versions {
        Versions {
            descriptor: self.descriptor.get(),
            groups: self.groups.get(),
            identify: self
                .identify
                .as_ref()
                .map_or(0, identify::ClusterHandler::dataver),
            on_off: self
                .on_off
                .as_ref()
                .map_or(0, on_off::ClusterAsyncHandler::dataver),
            bridged: self
                .bridged
                .as_ref()
                .map_or(0, bridged::ClusterHandler::dataver),
        }
    }
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn groups(&self) -> groups::GroupsHandler<'_> {
        groups::GroupsHandler::new_with_identify(
            Dataver::new(self.groups.get()),
            self.identify.as_ref().unwrap(),
        )
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        if self.endpoint == AGGREGATOR_ENDPOINT {
            Endpoint::new(
                self.endpoint,
                devices!(DEV_TYPE_AGGREGATOR),
                clusters!(desc::DescHandler::CLUSTER),
            )
        } else if self.on_off.is_some() && self.bridged.is_some() {
            Endpoint::new(self.endpoint, SINGLE_TYPES, SINGLE_CLUSTERS)
        } else if self.bridged.is_some() {
            Endpoint::new(
                self.endpoint,
                devices!(DEV_TYPE_BRIDGED_NODE),
                PARENT_CLUSTERS,
            )
        } else {
            Endpoint::new(self.endpoint, devices!(ON_OFF_PLUG), CHILD_CLUSTERS).with_tags(tags)
        }
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            0x1d => Handler::read(&self.descriptor().adapt(), ctx, reply),
            4 if self.identify.is_some() => Handler::read(&self.groups().adapt(), ctx, reply),
            3 if self.identify.is_some() => Handler::read(
                &identify::HandlerAdaptor(self.identify.as_ref().unwrap()),
                ctx,
                reply,
            ),
            6 if self.on_off.is_some() => {
                AsyncHandler::read(
                    &on_off::HandlerAsyncAdaptor(self.on_off.as_ref().unwrap()),
                    ctx,
                    reply,
                )
                .await
            }
            0x39 if self.bridged.is_some() => Handler::read(
                &bridged::HandlerAdaptor(self.bridged.as_ref().unwrap()),
                ctx,
                reply,
            ),
            _ => Err(ErrorCode::ClusterNotFound.into()),
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            3 if self.identify.is_some() => Handler::write(
                &identify::HandlerAdaptor(self.identify.as_ref().unwrap()),
                ctx,
            ),
            0x39 if self.bridged.is_some() => Handler::write(
                &bridged::HandlerAdaptor(self.bridged.as_ref().unwrap()),
                ctx,
            ),
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        match ctx.cmd().cluster_id {
            4 if self.identify.is_some() => Handler::invoke(&self.groups().adapt(), ctx, reply),
            3 if self.identify.is_some() => Handler::invoke(
                &identify::HandlerAdaptor(self.identify.as_ref().unwrap()),
                ctx,
                reply,
            ),
            6 if self.on_off.is_some() => {
                AsyncHandler::invoke(
                    &on_off::HandlerAsyncAdaptor(self.on_off.as_ref().unwrap()),
                    ctx,
                    reply,
                )
                .await
            }
            0x39 if self.bridged.is_some() => Handler::invoke(
                &bridged::HandlerAdaptor(self.bridged.as_ref().unwrap()),
                ctx,
                reply,
            ),
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }
    fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 0x1d) {
            self.descriptor.changed();
        }
        if cluster.is_none_or(|id| id == 4) && self.identify.is_some() {
            self.groups.changed();
        }
        if let Some(handler) = &self.identify
            && cluster.is_none_or(|id| id == 3)
        {
            handler.dataver_changed();
        }
        if let Some(handler) = &self.on_off
            && cluster.is_none_or(|id| id == 6)
        {
            handler.dataver_changed();
        }
        if let Some(handler) = &self.bridged
            && cluster.is_none_or(|id| id == 0x39)
        {
            handler.dataver_changed();
        }
    }
    fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
        if op == LifecycleOp::Startup && self.started.get() {
            return Ok(());
        }
        if let Some(handler) = &self.identify {
            Handler::lifecycle(&identify::HandlerAdaptor(handler), &ctx, op)?;
        }
        if self.identify.is_some() {
            Handler::lifecycle(&self.groups().adapt(), &ctx, op)?;
        }
        if let Some(handler) = &self.on_off {
            AsyncHandler::lifecycle(&on_off::HandlerAsyncAdaptor(handler), &ctx, op)?;
        }
        if let Some(handler) = &self.bridged {
            Handler::lifecycle(&bridged::HandlerAdaptor(handler), &ctx, op)?;
        }
        if op == LifecycleOp::Startup {
            self.started.set(true);
        }
        Ok(())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let identify = async {
            if let Some(handler) = &self.identify {
                identify::ClusterHandler::run(handler, &ctx).await
            } else {
                std::future::pending().await
            }
        };
        let on_off = async {
            if let Some(handler) = &self.on_off {
                on_off::ClusterAsyncHandler::run(handler, &ctx).await
            } else {
                std::future::pending().await
            }
        };
        let bridged = async {
            if let Some(handler) = &self.bridged {
                bridged::ClusterHandler::run(handler, &ctx).await
            } else {
                std::future::pending().await
            }
        };
        futures_lite::future::or(identify, futures_lite::future::or(on_off, bridged)).await
    }
}
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartsMatcher")
            .field("endpoint", &self.endpoint)
            .field("children", &self.children)
            .finish()
    }
}
impl DynBase for Entry {}
impl desc::PartsMatcher for Entry {
    fn matches(&self, _our_endpoint: u16, endpoint: u16) -> bool {
        if self.endpoint == AGGREGATOR_ENDPOINT {
            endpoint != 0 && endpoint != AGGREGATOR_ENDPOINT
        } else {
            self.children.contains(&endpoint)
        }
    }
}

pub(super) struct Model {
    registry: DeviceRegistry,
    store: Store,
    protocol: StoreAdapter,
    root: endpoints::EthSysHandler<'static>,
    entries: RefCell<Vec<Rc<Entry>>>,
    versions: RefCell<BTreeMap<u16, Versions>>,
}
pub(super) fn handler(
    registry: &DeviceRegistry,
    store: &Store,
    protocol: StoreAdapter,
    mut random: impl rand::Rng,
) -> Result<Model, StorageError> {
    let model = Model {
        registry: registry.clone(),
        store: store.clone(),
        protocol,
        root: endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(&mut random),
        entries: RefCell::new(vec![Rc::new(Entry {
            endpoint: AGGREGATOR_ENDPOINT,
            started: Cell::new(false),
            device: None,
            descriptor: Dataver::new_rand(&mut random),
            children: Vec::new(),
            tag: None,
            groups: Dataver::new_rand(&mut random),
            identify: None,
            on_off: None,
            bridged: None,
        })]),
        versions: RefCell::new(BTreeMap::new()),
    };
    model.reconcile(registry.list())?;
    Ok(model)
}
impl Model {
    fn entry(&self, endpoint: u16) -> Result<Rc<Entry>, Error> {
        self.entries
            .borrow()
            .iter()
            .find(|entry| entry.endpoint == endpoint)
            .cloned()
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }
    fn reconcile(&self, devices: Vec<PhysicalDevice>) -> Result<Vec<u16>, StorageError> {
        let old = self.entries.borrow().clone();
        let mut entries = vec![old[0].clone()];
        let mut changed = Vec::new();
        for device in devices {
            let stored = self
                .store
                .devices()
                .load_published(&device.description().key)?
                .ok_or_else(|| self.invalid("Canonical device has no published identity"))?;
            self.check(&device, &stored)?;
            let mut ids = vec![stored.endpoint];
            if stored.functions.len() > 1 {
                ids.extend(stored.functions.iter().map(|function| function.endpoint));
            }
            for endpoint in ids {
                if let Some(entry) = old.iter().find(|entry| {
                    entry.endpoint == endpoint
                        && entry
                            .device
                            .as_ref()
                            .is_some_and(|previous| previous.same_instance(&device))
                }) {
                    entries.push(entry.clone());
                    continue;
                }
                let previous = old
                    .iter()
                    .find(|entry| entry.endpoint == endpoint)
                    .map(|entry| entry.versions())
                    .or_else(|| self.versions.borrow().get(&endpoint).copied());
                let versions = previous.unwrap_or_else(|| {
                    let mut random = rand::rng();
                    Versions {
                        descriptor: random.random(),
                        groups: random.random(),
                        identify: random.random(),
                        on_off: random.random(),
                        bridged: random.random(),
                    }
                });
                let function = stored
                    .functions
                    .iter()
                    .find(|function| function.endpoint == endpoint);
                let control = function.map(|function| {
                    OnOffHandler::new(
                        device.function(&function.key.id).unwrap(),
                        endpoint,
                        Dataver::new(versions.on_off),
                    )
                });
                let physical = (endpoint == stored.endpoint).then(|| {
                    BridgedHandler::new(
                        device.clone(),
                        Dataver::new(versions.bridged),
                        &stored,
                        self.store.devices(),
                        self.protocol.clone(),
                    )
                });
                let tag = if stored.functions.len() > 1 {
                    function.map(|function| {
                        (
                            stored
                                .functions
                                .iter()
                                .position(|candidate| candidate.key == function.key)
                                .unwrap() as u8
                                + 1,
                            function.label.clone(),
                        )
                    })
                } else {
                    None
                };
                let children = if endpoint == stored.endpoint && stored.functions.len() > 1 {
                    stored
                        .functions
                        .iter()
                        .map(|function| function.endpoint)
                        .collect()
                } else {
                    Vec::new()
                };
                entries.push(Rc::new(Entry {
                    endpoint,
                    started: Cell::new(false),
                    device: Some(device.clone()),
                    descriptor: Dataver::new(versions.descriptor),
                    children,
                    tag,
                    groups: Dataver::new(versions.groups),
                    identify: control
                        .as_ref()
                        .map(|_| identify::IdentifyHandler::new(Dataver::new(versions.identify))),
                    on_off: control,
                    bridged: physical,
                }));
                changed.push(endpoint);
            }
        }
        for entry in old
            .iter()
            .filter(|entry| !entries.iter().any(|candidate| Rc::ptr_eq(entry, candidate)))
        {
            self.versions
                .borrow_mut()
                .insert(entry.endpoint, entry.versions());
            if !changed.contains(&entry.endpoint) {
                changed.push(entry.endpoint);
            }
        }
        entries.sort_by_key(|entry| entry.endpoint);
        *self.entries.borrow_mut() = entries;
        Ok(changed)
    }
    fn check(&self, device: &PhysicalDevice, stored: &StoredDevice) -> Result<(), StorageError> {
        let description = device.description();
        if description.functions.len() != stored.functions.len()
            || !stored.functions.iter().all(|stored| {
                description.functions.iter().any(|function| {
                    function.id == stored.key.id
                        && function.category == stored.category
                        && function.capabilities == stored.capabilities
                })
            })
        {
            return Err(self.invalid("Canonical functions differ from the published identity"));
        }
        Ok(())
    }
    fn invalid(&self, reason: &'static str) -> StorageError {
        StorageError::new(self.store.path(), "build Matter topology", reason)
    }
    async fn run_devices(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        type Job<'a> = (
            Rc<Entry>,
            Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>,
        );
        let mut changes = self.registry.subscribe();
        let mut jobs: Vec<Job<'_>> = Vec::new();
        let mut devices = self.registry.list();
        loop {
            let changed = self.protocol.record(self.reconcile(devices))?;
            let entries = self.entries.borrow().clone();
            jobs.retain(|(entry, _)| entries.iter().any(|current| Rc::ptr_eq(entry, current)));
            for entry in entries {
                if entry.endpoint == AGGREGATOR_ENDPOINT
                    || jobs.iter().any(|(running, _)| Rc::ptr_eq(running, &entry))
                {
                    continue;
                }
                entry.lifecycle(&ctx, LifecycleOp::Startup)?;
                let running = entry.clone();
                let context = &ctx;
                jobs.push((entry, Box::pin(async move { running.run(context).await })));
            }
            if !changed.is_empty() {
                for endpoint in changed {
                    ctx.notify_endpoint_changed(endpoint);
                }
                ctx.notify_attr_changed(0, 0x1d, desc::AttributeId::PartsList as _);
                ctx.notify_attr_changed(
                    AGGREGATOR_ENDPOINT,
                    0x1d,
                    desc::AttributeId::PartsList as _,
                );
            }
            let result = futures_lite::future::or(
                async { Ok::<_, Error>(changes.changed().await.devices) },
                async {
                    poll_fn(|cx| {
                        for (_, job) in &mut jobs {
                            if let Poll::Ready(result) = job.as_mut().poll(cx) {
                                return Poll::Ready(result);
                            }
                        }
                        Poll::Pending
                    })
                    .await?;
                    Err(ErrorCode::InvalidState.into())
                },
            )
            .await?;
            devices = result;
        }
    }
}
impl Metadata for Model {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let entries = self.entries.borrow();
        let tags = entries
            .iter()
            .map(|entry| {
                entry
                    .tag
                    .as_ref()
                    .map(|(tag, label)| vec![SemanticTag::new(0x07, *tag).with_label(label)])
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let mut endpoints = vec![ROOT];
        endpoints.extend(
            entries
                .iter()
                .zip(&tags)
                .map(|(entry, tags)| entry.metadata(tags)),
        );
        f(&Node::new(&endpoints))
    }
}
impl AsyncHandler for Model {
    fn read_awaits(&self, ctx: impl ReadContext) -> bool {
        ctx.attr().endpoint_id == 0 && self.root.read_awaits(ctx)
    }
    fn write_awaits(&self, ctx: impl WriteContext) -> bool {
        ctx.attr().endpoint_id == 0 && self.root.write_awaits(ctx)
    }
    fn invoke_awaits(&self, ctx: impl InvokeContext) -> bool {
        if ctx.cmd().endpoint_id == 0 {
            self.root.invoke_awaits(ctx)
        } else {
            ctx.cmd().cluster_id == 6
        }
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if ctx.attr().endpoint_id == 0 {
            self.root.read(ctx, reply).await
        } else {
            self.entry(ctx.attr().endpoint_id)?.read(ctx, reply).await
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if ctx.attr().endpoint_id == 0 {
            self.root.write(ctx).await
        } else {
            self.entry(ctx.attr().endpoint_id)?.write(ctx).await
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if ctx.cmd().endpoint_id == 0 {
            self.root.invoke(ctx, reply).await
        } else {
            self.entry(ctx.cmd().endpoint_id)?.invoke(ctx, reply).await
        }
    }
    fn bump_dataver(&self, ctx: impl MatchContext) {
        self.root.bump_dataver(&ctx);
        for entry in self.entries.borrow().iter().filter(|entry| {
            ctx.endpt()
                .is_none_or(|endpoint| entry.endpoint == endpoint)
        }) {
            entry.bump(ctx.cluster());
        }
    }
    fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
        self.root.lifecycle(&ctx, op)?;
        for entry in self.entries.borrow().iter() {
            entry.lifecycle(&ctx, op)?;
        }
        Ok(())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        futures_lite::future::or(self.root.run(&ctx), self.run_devices(&ctx)).await
    }
}
