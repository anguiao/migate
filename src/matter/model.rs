use super::{bridged_info::BridgedHandler, on_off::OnOffHandler, storage::StoreAdapter};
use crate::{
    device::{DeviceRegistry, Function, PhysicalDevice},
    storage::{Identity, StorageError, Store},
};
use rand::RngExt as _;
use rs_matter::{
    Matter, clusters, devices,
    dm::{
        Async, AsyncHandler, ChainedHandler, Dataver, DeviceType, EmptyHandler, Endpoint,
        FnMatcher, Handler, HandlerContext, InvokeContext, InvokeReply, LifecycleOp, MatchContext,
        Metadata, Node, ReadContext, ReadReply, SemanticTag, WriteContext,
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
    cell::RefCell,
    collections::BTreeMap,
    future::{Future, poll_fn},
    rc::Rc,
    task::Poll,
};

const AGGREGATOR_ENDPOINT: u16 = 1;
const ROOT: Endpoint<'static> = root_endpoint!(eth);
const AGGREGATOR: Endpoint<'static> = Endpoint::new(
    AGGREGATOR_ENDPOINT,
    devices!(DEV_TYPE_AGGREGATOR),
    clusters!(desc::DescHandler::CLUSTER),
);
type RootHandler = ChainedHandler<
    FnMatcher,
    Async<desc::HandlerAdaptor<desc::DescHandler<'static>>>,
    endpoints::EthSysHandler<'static>,
>;
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
    device: PhysicalDevice,
    descriptor: Dataver,
    role: Role,
}
enum Role {
    Single {
        control: Control,
        bridged: BridgedHandler,
    },
    Parent {
        children: Vec<u16>,
        bridged: BridgedHandler,
    },
    Child {
        control: Control,
        tag: u8,
        label: String,
    },
}
struct Control {
    groups: Dataver,
    identify: identify::IdentifyHandler,
    on_off: OnOffHandler,
}
impl Control {
    fn new(function: Function, endpoint: u16, versions: Versions) -> Self {
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            on_off: OnOffHandler::new(function, endpoint, Dataver::new(versions.on_off)),
        }
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        EmptyHandler
            .chain(
                |_, cluster| cluster == 4,
                Async(
                    groups::GroupsHandler::new_with_identify(
                        Dataver::new(self.groups.get()),
                        &self.identify,
                    )
                    .adapt(),
                ),
            )
            .chain(
                |_, cluster| cluster == 3,
                Async(identify::HandlerAdaptor(&self.identify)),
            )
            .chain(
                |_, cluster| cluster == 6,
                on_off::HandlerAsyncAdaptor(&self.on_off),
            )
    }
}
impl Role {
    fn control(&self) -> Option<&Control> {
        match self {
            Self::Single { control, .. } | Self::Child { control, .. } => Some(control),
            Self::Parent { .. } => None,
        }
    }
    fn bridged(&self) -> Option<&BridgedHandler> {
        match self {
            Self::Single { bridged, .. } | Self::Parent { bridged, .. } => Some(bridged),
            Self::Child { .. } => None,
        }
    }
}
// Each role composes only its supported clusters; rs-matter owns operation routing.
macro_rules! dispatch {
    ($entry:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match &$entry.role {
            Role::Single { control, bridged, .. } => AsyncHandler::$method(
                &ChainedHandler::new(
                    |_, cluster| cluster == 0x39,
                    Async(bridged::HandlerAdaptor(bridged)),
                    control.handler(),
                ), $($arg),*
            ).await,
            Role::Parent { bridged, .. } => AsyncHandler::$method(
                &EmptyHandler.chain(
                    |_, cluster| cluster == 0x39,
                    Async(bridged::HandlerAdaptor(bridged)),
                ), $($arg),*
            ).await,
            Role::Child { control, .. } => control.handler().$method($($arg),*).await,
        }
    };
}

impl Entry {
    fn versions(&self) -> Versions {
        let control = self.role.control();
        Versions {
            descriptor: self.descriptor.get(),
            groups: control.map_or(0, |control| control.groups.get()),
            identify: control.map_or(0, |control| control.identify.dataver()),
            on_off: control.map_or(0, |control| control.on_off.dataver()),
            bridged: self
                .role
                .bridged()
                .map_or(0, bridged::ClusterHandler::dataver),
        }
    }
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        match &self.role {
            Role::Single { .. } => Endpoint::new(self.endpoint, SINGLE_TYPES, SINGLE_CLUSTERS),
            Role::Parent { .. } => Endpoint::new(
                self.endpoint,
                devices!(DEV_TYPE_BRIDGED_NODE),
                PARENT_CLUSTERS,
            ),
            Role::Child { .. } => {
                Endpoint::new(self.endpoint, devices!(ON_OFF_PLUG), CHILD_CLUSTERS).with_tags(tags)
            }
        }
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if ctx.attr().cluster_id == 0x1d {
            Handler::read(&self.descriptor().adapt(), ctx, reply)
        } else {
            dispatch!(self, read(ctx, reply))
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        dispatch!(self, write(ctx))
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        dispatch!(self, invoke(ctx, reply))
    }
    fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 0x1d) {
            self.descriptor.changed();
        }
        if let Some(control) = self.role.control() {
            if cluster.is_none_or(|id| id == 4) {
                control.groups.changed();
            }
            if cluster.is_none_or(|id| id == 3) {
                control.identify.dataver_changed();
            }
            if cluster.is_none_or(|id| id == 6) {
                control.on_off.dataver_changed();
            }
        }
        if let Some(handler) = self.role.bridged()
            && cluster.is_none_or(|id| id == 0x39)
        {
            handler.dataver_changed();
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        dispatch!(self, run(ctx))
    }
}
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartsMatcher")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}
impl DynBase for Entry {}
impl desc::PartsMatcher for Entry {
    fn matches(&self, _our_endpoint: u16, endpoint: u16) -> bool {
        match &self.role {
            Role::Parent { children, .. } => children.contains(&endpoint),
            Role::Single { .. } | Role::Child { .. } => false,
        }
    }
}

pub(super) struct Model {
    registry: DeviceRegistry,
    store: Store,
    protocol: StoreAdapter,
    root: RootHandler,
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
            .build(&mut random)
            .chain(
                |endpoint, cluster| endpoint == AGGREGATOR_ENDPOINT && cluster == 0x1d,
                Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
            ),
        entries: RefCell::new(Vec::new()),
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
        let mut entries = Vec::new();
        let mut changed = Vec::new();
        for device in devices {
            let before = entries.len();
            entries.extend(
                old.iter()
                    .filter(|entry| entry.device.same_instance(&device))
                    .cloned(),
            );
            if entries.len() != before {
                continue;
            }
            let stored = self
                .store
                .devices()
                .load_published(device.key())?
                .ok_or_else(|| self.invalid("Canonical device has no published identity"))?;
            let multiple = stored.functions.len() > 1;
            let parent = multiple.then_some((stored.endpoint, None));
            let functions = stored
                .functions
                .iter()
                .enumerate()
                .map(|(index, function)| (function.endpoint, Some((index, function))));
            for (endpoint, function) in parent.into_iter().chain(functions) {
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
                let bridged = || {
                    BridgedHandler::new(
                        device.clone(),
                        Dataver::new(versions.bridged),
                        &stored,
                        self.store.devices(),
                        self.protocol.clone(),
                    )
                };
                let role = match function {
                    None => Role::Parent {
                        children: stored
                            .functions
                            .iter()
                            .map(|function| function.endpoint)
                            .collect(),
                        bridged: bridged(),
                    },
                    Some((index, function)) => {
                        let control = Control::new(
                            device.function(&function.key.id).unwrap(),
                            endpoint,
                            versions,
                        );
                        if multiple {
                            Role::Child {
                                control,
                                tag: index as u8 + 1,
                                label: function.label.clone(),
                            }
                        } else {
                            Role::Single {
                                control,
                                bridged: bridged(),
                            }
                        }
                    }
                };
                entries.push(Rc::new(Entry {
                    endpoint,
                    device: device.clone(),
                    descriptor: Dataver::new(versions.descriptor),
                    role,
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
    fn invalid(&self, reason: &'static str) -> StorageError {
        StorageError::new(self.store.path(), "build Matter topology", reason)
    }
    async fn run_devices(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.registry.subscribe();
        let mut jobs: Vec<(Rc<Entry>, _)> = Vec::new();
        let mut devices = self.registry.list();
        loop {
            let changed = self.protocol.record(self.reconcile(devices))?;
            let entries = self.entries.borrow().clone();
            jobs.retain(|(entry, _)| entries.iter().any(|current| Rc::ptr_eq(entry, current)));
            for entry in entries {
                if jobs.iter().any(|(running, _)| Rc::ptr_eq(running, &entry)) {
                    continue;
                }
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
            .map(|entry| match &entry.role {
                Role::Child { tag, label, .. } => {
                    vec![SemanticTag::new(0x07, *tag).with_label(label)]
                }
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut endpoints = vec![ROOT, AGGREGATOR];
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
        ctx.attr().endpoint_id <= AGGREGATOR_ENDPOINT && self.root.read_awaits(ctx)
    }
    fn write_awaits(&self, ctx: impl WriteContext) -> bool {
        ctx.attr().endpoint_id <= AGGREGATOR_ENDPOINT && self.root.write_awaits(ctx)
    }
    fn invoke_awaits(&self, ctx: impl InvokeContext) -> bool {
        if ctx.cmd().endpoint_id <= AGGREGATOR_ENDPOINT {
            self.root.invoke_awaits(ctx)
        } else {
            ctx.cmd().cluster_id == 6
        }
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if ctx.attr().endpoint_id <= AGGREGATOR_ENDPOINT {
            self.root.read(ctx, reply).await
        } else {
            self.entry(ctx.attr().endpoint_id)?.read(ctx, reply).await
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if ctx.attr().endpoint_id <= AGGREGATOR_ENDPOINT {
            self.root.write(ctx).await
        } else {
            self.entry(ctx.attr().endpoint_id)?.write(ctx).await
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if ctx.cmd().endpoint_id <= AGGREGATOR_ENDPOINT {
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
        // Dynamic clusters keep no persistent lifecycle state.
        self.root.lifecycle(ctx, op)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        futures_lite::future::or(self.root.run(&ctx), self.run_devices(&ctx)).await
    }
}
