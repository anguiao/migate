use super::{
    bridged_info::{BridgedHandler, GroupBridge},
    lighting,
    on_off::OnOffHandler,
    storage::StoreAdapter,
};
use crate::{
    device::{Category, DeviceKey, DeviceRegistry, Function, PhysicalDevice, RegistrySnapshot},
    storage::{Identity, StorageError, Store, StoredDevice},
};
use futures_util::{
    StreamExt,
    future::{AbortHandle, Abortable, LocalBoxFuture},
    stream::FuturesUnordered,
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
            decl::{
                bridged_device_basic_information::{self as bridged, ClusterHandler as _},
                color_control, level_control,
            },
            desc::{self, ClusterHandler as _},
            groups::{self, ClusterHandler as _},
            identify::{self, ClusterHandler as _},
        },
        devices::{
            DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE,
            test::{TEST_DEV_DET, TEST_VID},
        },
        endpoints,
        networks::SysNetifs,
    },
    error::{Error, ErrorCode},
    root_endpoint,
    utils::sync::DynBase,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
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
const CT_LIGHT: DeviceType = DeviceType {
    dtype: 0x010c,
    drev: 4,
};
const RGB_LIGHT: DeviceType = DeviceType {
    dtype: 0x010d,
    drev: 4,
};
const LIGHT_IDENTIFY: rs_matter::dm::Cluster<'static> = identify::IdentifyHandler::<()>::CLUSTER
    .with_cmds(rs_matter::with!(identify::CommandId::Identify));
const CT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    LIGHT_IDENTIFY,
    BridgedHandler::CLUSTER,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER
);
const RGB_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    LIGHT_IDENTIFY,
    BridgedHandler::CLUSTER,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER
);
// A composition member keeps its full light surface but owns no bridge identity.
const CT_CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    LIGHT_IDENTIFY,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER
);
const RGB_CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    LIGHT_IDENTIFY,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER
);
const PARENT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] =
    clusters!(desc::DescHandler::CLUSTER, BridgedHandler::CLUSTER);
const CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    OnOffHandler::CLUSTER
);
/// Standard "Common Number" namespace. Matter Core 1.3 requires at least one
/// standard tag next to manufacturer-specific ones; 0 is a defined value.
const NS_COMMON_NUMBER: u8 = 0x07;

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
    level: u32,
    color: u32,
}
struct Entry {
    endpoint: u16,
    device: Option<PhysicalDevice>,
    descriptor: Dataver,
    role: Role,
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum ChildTags {
    Outlet { index: u8, label: String },
    Light { endpoint: u16, label: String },
}
enum Role {
    Single {
        control: Rc<Control>,
        bridged: Rc<BridgedHandler>,
    },
    Parent {
        children: Vec<u16>,
        bridged: Rc<BridgedHandler>,
    },
    Child {
        control: Rc<Control>,
        tags: ChildTags,
        /// The bridge version is retained while the cluster is hidden so a
        /// member that leaves its group resumes its previous identity version.
        bridged_version: Dataver,
    },
}
struct Control {
    groups: Dataver,
    identify: identify::IdentifyHandler,
    on_off: Option<OnOffHandler>,
    lighting: Option<lighting::Lighting>,
}
impl Control {
    fn new(function: Function, endpoint: u16, versions: Versions) -> Self {
        let is_light = function.description().category == Category::Light;
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            on_off: (!is_light).then(|| {
                OnOffHandler::new(function.clone(), endpoint, Dataver::new(versions.on_off))
            }),
            lighting: is_light.then(|| {
                lighting::Lighting::new(
                    function,
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                )
                .expect("validated published lighting capabilities")
            }),
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
                on_off::HandlerAsyncAdaptor(self.on_off.as_ref().unwrap()),
            )
    }
    fn light_handler(&self) -> impl AsyncHandler + '_ {
        let light = self.lighting.as_ref().unwrap();
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
                on_off::HandlerAsyncAdaptor(lighting::OnOffHandler(light)),
            )
            .chain(
                |_, cluster| cluster == 8,
                level_control::HandlerAsyncAdaptor(lighting::LevelHandler(light)),
            )
            .chain(|_, cluster| cluster == 0x300, ColorDispatch(light))
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if self.lighting.is_some() {
            self.light_handler().read(ctx, reply).await
        } else {
            self.handler().read(ctx, reply).await
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if self.lighting.is_some() {
            self.light_handler().write(ctx).await
        } else {
            self.handler().write(ctx).await
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if let Some(light) = &self.lighting {
            let commands = match ctx.cmd().cluster_id {
                3 => Some(&LIGHT_IDENTIFY),
                6 => Some(&lighting::OnOffHandler::CLUSTER),
                8 => Some(&<lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER),
                0x300 if light.rgb() => Some(
                    &<lighting::ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER,
                ),
                0x300 => Some(
                    &<lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER,
                ),
                _ => None,
            };
            if commands.is_some_and(|cluster| cluster.command(ctx.cmd().cmd_id).is_none()) {
                return Err(ErrorCode::CommandNotFound.into());
            }
            self.light_handler().invoke(ctx, reply).await
        } else {
            self.handler().invoke(ctx, reply).await
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        if self.lighting.is_some() {
            self.light_handler().run(ctx).await
        } else {
            self.handler().run(ctx).await
        }
    }
    fn on_off_version(&self) -> u32 {
        self.lighting.as_ref().map_or_else(
            || self.on_off.as_ref().unwrap().dataver(),
            |l| l.on_off_version.get(),
        )
    }
}
struct ColorDispatch<'a>(&'a lighting::Lighting);
impl AsyncHandler for ColorDispatch<'_> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if self.0.rgb() {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                .read(ctx, reply)
                .await
        } else {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                .read(ctx, reply)
                .await
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if self.0.rgb() {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                .write(ctx)
                .await
        } else {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                .write(ctx)
                .await
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if self.0.rgb() {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                .invoke(ctx, reply)
                .await
        } else {
            color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                .invoke(ctx, reply)
                .await
        }
    }
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
}
struct ControlHandler<'a>(&'a Control);
impl AsyncHandler for ControlHandler<'_> {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.0.read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.0.write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        self.0.invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.0.run(ctx).await
    }
}
impl Role {
    fn control(&self) -> Option<&Control> {
        match self {
            Self::Single { control, .. } | Self::Child { control, .. } => Some(control),
            Self::Parent { .. } => None,
        }
    }
    fn control_rc(&self) -> Option<Rc<Control>> {
        match self {
            Self::Single { control, .. } | Self::Child { control, .. } => Some(control.clone()),
            Self::Parent { .. } => None,
        }
    }
    fn bridged(&self) -> Option<&BridgedHandler> {
        match self {
            Self::Single { bridged, .. } | Self::Parent { bridged, .. } => Some(bridged),
            Self::Child { .. } => None,
        }
    }
    fn bridged_rc(&self) -> Option<Rc<BridgedHandler>> {
        match self {
            Self::Single { bridged, .. } | Self::Parent { bridged, .. } => Some(bridged.clone()),
            Self::Child { .. } => None,
        }
    }
    fn bridged_version(&self) -> u32 {
        match self {
            Self::Single { bridged, .. } | Self::Parent { bridged, .. } => bridged.dataver(),
            Self::Child {
                bridged_version, ..
            } => bridged_version.get(),
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
                    Async(bridged::HandlerAdaptor(bridged.as_ref())),
                    &ControlHandler(control),
                ), $($arg),*
            ).await,
            Role::Parent { bridged, .. } => AsyncHandler::$method(
                &EmptyHandler.chain(
                    |_, cluster| cluster == 0x39,
                    Async(bridged::HandlerAdaptor(bridged.as_ref())),
                ), $($arg),*
            ).await,
            Role::Child { control, .. } => control.$method($($arg),*).await,
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
            on_off: control.map_or(0, Control::on_off_version),
            level: control
                .and_then(|c| c.lighting.as_ref())
                .map_or(0, |l| l.level_version.get()),
            color: control
                .and_then(|c| c.lighting.as_ref())
                .map_or(0, |l| l.color_version.get()),
            bridged: self.role.bridged_version(),
        }
    }
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        match &self.role {
            Role::Single { control, .. } => {
                if let Some(light) = &control.lighting {
                    if light.rgb() {
                        Endpoint::new(
                            self.endpoint,
                            devices!(RGB_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            RGB_CLUSTERS,
                        )
                    } else {
                        Endpoint::new(
                            self.endpoint,
                            devices!(CT_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            CT_CLUSTERS,
                        )
                    }
                } else {
                    Endpoint::new(self.endpoint, SINGLE_TYPES, SINGLE_CLUSTERS)
                }
            }
            Role::Parent { .. } => Endpoint::new(
                self.endpoint,
                devices!(DEV_TYPE_BRIDGED_NODE),
                PARENT_CLUSTERS,
            ),
            Role::Child {
                control,
                tags: kind,
                ..
            } => match kind {
                ChildTags::Outlet { .. } => {
                    Endpoint::new(self.endpoint, devices!(ON_OFF_PLUG), CHILD_CLUSTERS)
                        .with_tags(tags)
                }
                ChildTags::Light { .. } => {
                    let rgb = control
                        .lighting
                        .as_ref()
                        .expect("light member has lighting capabilities")
                        .rgb();
                    if rgb {
                        Endpoint::new(self.endpoint, devices!(RGB_LIGHT), RGB_CHILD_CLUSTERS)
                            .with_tags(tags)
                    } else {
                        Endpoint::new(self.endpoint, devices!(CT_LIGHT), CT_CHILD_CLUSTERS)
                            .with_tags(tags)
                    }
                }
            },
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
                if let Some(light) = &control.lighting {
                    light.on_off_version.changed();
                } else {
                    control.on_off.as_ref().unwrap().dataver_changed();
                }
            }
        }
        if let Some(light) = self.role.control().and_then(|c| c.lighting.as_ref()) {
            if cluster.is_none_or(|id| id == 8) {
                light.level_version.changed();
            }
            if cluster.is_none_or(|id| id == 0x300) {
                light.color_version.changed();
            }
        }
        if let Some(handler) = self.role.bridged()
            && cluster.is_none_or(|id| id == 0x39)
        {
            handler.dataver_changed();
        }
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

#[derive(Default)]
struct Reconcile {
    added: Vec<u16>,
    removed: Vec<u16>,
    /// Existing endpoints whose descriptor metadata changed.
    metadata: Vec<u16>,
    /// Existing endpoints where the Bridged Device Basic Information cluster
    /// appeared or disappeared, so its subscriptions must be re-evaluated.
    bridged_presence: Vec<u16>,
    /// Existing endpoints whose device instance was replaced; their whole
    /// control surface may differ, so every cluster version is reported.
    replaced: Vec<u16>,
    /// Restart one control job; `None` stops it.
    controls: Vec<(u16, Option<Rc<Control>>)>,
    /// Restart one bridge job; `None` stops it.
    bridges: Vec<(u16, Option<Rc<BridgedHandler>>)>,
}

type JobResult = Result<Result<(), Error>, futures_util::future::Aborted>;
type JobFutures<'a> = FuturesUnordered<LocalBoxFuture<'a, JobResult>>;

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
    model.reconcile(&registry.snapshot())?;
    Ok(model)
}

fn same_instance(entry: &Entry, device: &PhysicalDevice) -> bool {
    entry
        .device
        .as_ref()
        .is_some_and(|present| present.same_instance(device))
}
fn same_control(left: &Option<Rc<Control>>, right: &Option<Rc<Control>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Rc::ptr_eq(left, right),
        _ => false,
    }
}
fn same_bridge(left: &Option<Rc<BridgedHandler>>, right: &Option<Rc<BridgedHandler>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Rc::ptr_eq(left, right),
        _ => false,
    }
}
fn same_metadata(previous: &Entry, desired: &Entry) -> bool {
    let same_device = match (&previous.device, &desired.device) {
        (Some(left), Some(right)) => left.same_instance(right),
        (None, None) => true,
        _ => false,
    };
    if !same_device {
        return false;
    }
    match (&previous.role, &desired.role) {
        (Role::Single { .. }, Role::Single { .. }) => true,
        (
            Role::Parent {
                children: left,
                bridged: left_bridge,
                ..
            },
            Role::Parent {
                children: right,
                bridged: right_bridge,
                ..
            },
        ) => left == right && left_bridge.group_key() == right_bridge.group_key(),
        (Role::Child { tags: left, .. }, Role::Child { tags: right, .. }) => left == right,
        _ => false,
    }
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
    fn versions_at(&self, previous: &BTreeMap<u16, Rc<Entry>>, endpoint: u16) -> Versions {
        previous
            .get(&endpoint)
            .map(|entry| entry.versions())
            .or_else(|| self.versions.borrow().get(&endpoint).copied())
            .unwrap_or_else(|| {
                let mut random = rand::rng();
                Versions {
                    descriptor: random.random(),
                    groups: random.random(),
                    identify: random.random(),
                    on_off: random.random(),
                    bridged: random.random(),
                    level: random.random(),
                    color: random.random(),
                }
            })
    }
    fn reconcile(&self, snapshot: &RegistrySnapshot) -> Result<Reconcile, StorageError> {
        let previous = self.entries.borrow().clone();
        let previous_by_endpoint = previous
            .iter()
            .map(|entry| (entry.endpoint, entry.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut stored = HashMap::<DeviceKey, StoredDevice>::new();
        for device in &snapshot.devices {
            let identity = self
                .store
                .devices()
                .load_published(device.key())?
                .ok_or_else(|| self.invalid("Canonical device has no published identity"))?;
            stored.insert(device.key().clone(), identity);
        }
        let mut member_group = HashMap::<DeviceKey, DeviceKey>::new();
        for group in &snapshot.composition.groups {
            for member in &group.members {
                member_group.insert(member.clone(), group.key.clone());
            }
        }

        let mut desired = Vec::<Rc<Entry>>::new();
        // Shared group state is only mutated after every fallible step passed,
        // so a later failure cannot leave the old topology with new members.
        let mut member_updates = Vec::<(Rc<GroupBridge>, Vec<PhysicalDevice>)>::new();
        for device in &snapshot.devices {
            let identity = &stored[device.key()];
            let multiple = identity.functions.len() > 1;
            if multiple {
                let endpoint = identity.endpoint;
                let versions = self.versions_at(&previous_by_endpoint, endpoint);
                let inherited = previous_by_endpoint
                    .get(&endpoint)
                    .filter(|entry| same_instance(entry, device));
                let bridged = match inherited.map(|entry| &entry.role) {
                    Some(Role::Parent { bridged, .. }) => bridged.clone(),
                    _ => Rc::new(BridgedHandler::new(
                        device.clone(),
                        Dataver::new(versions.bridged),
                        identity,
                        self.store.devices(),
                        self.protocol.clone(),
                    )),
                };
                desired.push(Rc::new(Entry {
                    endpoint,
                    device: Some(device.clone()),
                    descriptor: Dataver::new(versions.descriptor),
                    role: Role::Parent {
                        children: identity.functions.iter().map(|f| f.endpoint).collect(),
                        bridged,
                    },
                }));
                for (index, function) in identity.functions.iter().enumerate() {
                    let endpoint = function.endpoint;
                    let versions = self.versions_at(&previous_by_endpoint, endpoint);
                    let inherited = previous_by_endpoint
                        .get(&endpoint)
                        .filter(|entry| same_instance(entry, device));
                    let control = inherited
                        .and_then(|entry| entry.role.control_rc())
                        .unwrap_or_else(|| {
                            Rc::new(Control::new(
                                device.function(&function.key.id).unwrap(),
                                endpoint,
                                versions,
                            ))
                        });
                    desired.push(Rc::new(Entry {
                        endpoint,
                        device: Some(device.clone()),
                        descriptor: Dataver::new(versions.descriptor),
                        role: Role::Child {
                            control,
                            tags: ChildTags::Outlet {
                                index: index as u8 + 1,
                                label: function.label.clone(),
                            },
                            bridged_version: Dataver::new(versions.bridged),
                        },
                    }));
                }
            } else {
                let function = &identity.functions[0];
                let endpoint = function.endpoint;
                let versions = self.versions_at(&previous_by_endpoint, endpoint);
                let inherited = previous_by_endpoint
                    .get(&endpoint)
                    .filter(|entry| same_instance(entry, device));
                let control = inherited
                    .and_then(|entry| entry.role.control_rc())
                    .unwrap_or_else(|| {
                        Rc::new(Control::new(
                            device.function(&function.key.id).unwrap(),
                            endpoint,
                            versions,
                        ))
                    });
                if member_group.contains_key(device.key()) {
                    desired.push(Rc::new(Entry {
                        endpoint,
                        device: Some(device.clone()),
                        descriptor: Dataver::new(versions.descriptor),
                        role: Role::Child {
                            control,
                            tags: ChildTags::Light {
                                endpoint,
                                label: identity.label.clone(),
                            },
                            bridged_version: Dataver::new(versions.bridged),
                        },
                    }));
                } else {
                    let bridged = match inherited.map(|entry| &entry.role) {
                        Some(Role::Single { bridged, .. }) => bridged.clone(),
                        _ => Rc::new(BridgedHandler::new(
                            device.clone(),
                            Dataver::new(versions.bridged),
                            identity,
                            self.store.devices(),
                            self.protocol.clone(),
                        )),
                    };
                    desired.push(Rc::new(Entry {
                        endpoint,
                        device: Some(device.clone()),
                        descriptor: Dataver::new(versions.descriptor),
                        role: Role::Single { control, bridged },
                    }));
                }
            }
        }

        for group in &snapshot.composition.groups {
            let group_identity = self
                .store
                .devices()
                .load_group_identity(&group.key)?
                .ok_or_else(|| self.invalid("Group composition has no stored identity"))?;
            let endpoint = group_identity.endpoint;
            let mut members = Vec::new();
            let mut children = Vec::new();
            for member in &group.members {
                let device = snapshot
                    .devices
                    .iter()
                    .find(|device| device.key() == member)
                    .cloned()
                    .ok_or_else(|| self.invalid("Group member is not in the registry"))?;
                let Some(identity) = stored.get(member) else {
                    return Err(self.invalid("Group member has no published identity"));
                };
                let [function] = identity.functions.as_slice() else {
                    return Err(self.invalid("Group member does not have exactly one function"));
                };
                children.push(function.endpoint);
                members.push(device);
            }
            children.sort_unstable();
            let versions = self.versions_at(&previous_by_endpoint, endpoint);
            let previous_group = previous_by_endpoint.get(&endpoint).and_then(|entry| {
                match (&entry.role, &entry.device) {
                    (Role::Parent { children, bridged }, None)
                        if bridged.group_key() == Some(&group.key) =>
                    {
                        Some((children.clone(), bridged.clone()))
                    }
                    _ => None,
                }
            });
            let (bridged, staged) = match previous_group {
                Some((old_children, old_bridged)) => {
                    let bridge = old_bridged
                        .group_state()
                        .expect("group bridge owns shared state");
                    if old_children == children && bridge.same_members(&members) {
                        (old_bridged, None)
                    } else {
                        let handler = Rc::new(BridgedHandler::new_group(
                            bridge.clone(),
                            Dataver::new(versions.bridged),
                            self.store.devices(),
                            self.protocol.clone(),
                        ));
                        (handler, Some((bridge, members)))
                    }
                }
                None => {
                    let bridge = Rc::new(GroupBridge::new(
                        group.key.clone(),
                        group_identity.unique_id.clone(),
                        endpoint,
                        group_identity.label.clone(),
                        members,
                        self.registry.clone(),
                    ));
                    let handler = Rc::new(BridgedHandler::new_group(
                        bridge,
                        Dataver::new(versions.bridged),
                        self.store.devices(),
                        self.protocol.clone(),
                    ));
                    (handler, None)
                }
            };
            if let Some((bridge, members)) = staged {
                member_updates.push((bridge, members));
            }
            desired.push(Rc::new(Entry {
                endpoint,
                device: None,
                descriptor: Dataver::new(versions.descriptor),
                role: Role::Parent {
                    children: children.clone(),
                    bridged,
                },
            }));
        }

        desired.sort_by_key(|entry| entry.endpoint);
        for pair in desired.windows(2) {
            if pair[0].endpoint == pair[1].endpoint {
                return Err(self.invalid("Reconciled endpoints are duplicated"));
            }
        }
        for (bridge, members) in member_updates {
            bridge.set_members(members);
        }
        let endpoints = desired
            .iter()
            .map(|entry| entry.endpoint)
            .collect::<Vec<_>>();
        let mut report = Reconcile::default();
        for entry in &desired {
            let previous = previous_by_endpoint.get(&entry.endpoint);
            match previous {
                None => report.added.push(entry.endpoint),
                Some(previous) if !same_metadata(previous, entry) => {
                    let replaced = match (&previous.device, &entry.device) {
                        (Some(previous), Some(desired)) => !previous.same_instance(desired),
                        _ => false,
                    };
                    if replaced {
                        report.replaced.push(entry.endpoint);
                    } else {
                        report.metadata.push(entry.endpoint);
                        if previous.role.bridged_rc().is_some() != entry.role.bridged_rc().is_some()
                        {
                            report.bridged_presence.push(entry.endpoint);
                        }
                    }
                }
                Some(_) => {}
            }
            let control = entry.role.control_rc();
            if !same_control(
                &control,
                &previous.and_then(|entry| entry.role.control_rc()),
            ) {
                report.controls.push((entry.endpoint, control));
            }
            let bridged = entry.role.bridged_rc();
            if !same_bridge(
                &bridged,
                &previous.and_then(|entry| entry.role.bridged_rc()),
            ) {
                report.bridges.push((entry.endpoint, bridged));
            }
        }
        for entry in &previous {
            if !endpoints.contains(&entry.endpoint) {
                report.removed.push(entry.endpoint);
                if entry.role.control_rc().is_some() {
                    report.controls.push((entry.endpoint, None));
                }
                if entry.role.bridged_rc().is_some() {
                    report.bridges.push((entry.endpoint, None));
                }
                self.versions
                    .borrow_mut()
                    .insert(entry.endpoint, entry.versions());
            }
        }
        *self.entries.borrow_mut() = desired;
        Ok(report)
    }
    fn invalid(&self, reason: &'static str) -> StorageError {
        StorageError::new(self.store.path(), "build Matter topology", reason)
    }
    fn start_control<'a>(
        ctx: &'a impl HandlerContext,
        tasks: &mut JobFutures<'a>,
        control: Rc<Control>,
    ) -> AbortHandle {
        let (handle, registration) = AbortHandle::new_pair();
        tasks.push(Box::pin(Abortable::new(
            async move { ControlHandler(&control).run(ctx).await },
            registration,
        )));
        handle
    }
    fn start_bridge<'a>(
        ctx: &'a impl HandlerContext,
        tasks: &mut JobFutures<'a>,
        bridge: Rc<BridgedHandler>,
    ) -> AbortHandle {
        let (handle, registration) = AbortHandle::new_pair();
        tasks.push(Box::pin(Abortable::new(
            async move { bridged::HandlerAdaptor(bridge.as_ref()).run(ctx).await },
            registration,
        )));
        handle
    }
    async fn run_devices(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.registry.subscribe();
        let mut control_jobs = BTreeMap::<u16, AbortHandle>::new();
        let mut bridge_jobs = BTreeMap::<u16, AbortHandle>::new();
        let mut tasks = JobFutures::new();
        let mut snapshot = self.registry.snapshot();
        loop {
            let report = self.protocol.record(self.reconcile(&snapshot))?;
            for (endpoint, control) in report.controls {
                if let Some(job) = control_jobs.remove(&endpoint) {
                    job.abort();
                }
                if let Some(control) = control {
                    control_jobs.insert(endpoint, Self::start_control(&ctx, &mut tasks, control));
                }
            }
            for (endpoint, bridge) in report.bridges {
                if let Some(job) = bridge_jobs.remove(&endpoint) {
                    job.abort();
                }
                if let Some(bridge) = bridge {
                    bridge_jobs.insert(endpoint, Self::start_bridge(&ctx, &mut tasks, bridge));
                }
            }
            // `handler` already reconciled the initial topology, so the first
            // report is empty. Seed the jobs of every current role explicitly
            // so a reused entry that never appears in a report still runs.
            for entry in self.entries.borrow().iter() {
                if let Some(control) = entry.role.control_rc()
                    && !control_jobs.contains_key(&entry.endpoint)
                {
                    control_jobs.insert(
                        entry.endpoint,
                        Self::start_control(&ctx, &mut tasks, control),
                    );
                }
                if let Some(bridge) = entry.role.bridged_rc()
                    && !bridge_jobs.contains_key(&entry.endpoint)
                {
                    bridge_jobs
                        .insert(entry.endpoint, Self::start_bridge(&ctx, &mut tasks, bridge));
                }
            }
            for endpoint in &report.metadata {
                ctx.notify_cluster_changed(*endpoint, 0x1d);
            }
            // A role change adds or removes Bridged Device Basic Information
            // on an existing endpoint. Report that cluster explicitly so
            // Reachable/NodeLabel subscriptions are not left stale, while no
            // light or control cluster version changes.
            for endpoint in &report.bridged_presence {
                ctx.notify_cluster_changed(*endpoint, 0x39);
            }
            for endpoint in &report.replaced {
                ctx.notify_endpoint_changed(*endpoint);
            }
            for endpoint in &report.added {
                ctx.notify_endpoint_changed(*endpoint);
            }
            for endpoint in &report.removed {
                ctx.notify_endpoint_changed(*endpoint);
            }
            if !report.added.is_empty() || !report.removed.is_empty() {
                ctx.notify_attr_changed(0, 0x1d, desc::AttributeId::PartsList as _);
                ctx.notify_attr_changed(
                    AGGREGATOR_ENDPOINT,
                    0x1d,
                    desc::AttributeId::PartsList as _,
                );
            }
            let result = futures_lite::future::or(
                async { Ok::<_, Error>(changes.changed().await) },
                async {
                    while let Some(result) = tasks.next().await {
                        if let Ok(result) = result {
                            result?;
                            return Err(ErrorCode::InvalidState.into());
                        }
                    }
                    std::future::pending().await
                },
            )
            .await?;
            snapshot = result;
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
                Role::Child {
                    tags: ChildTags::Outlet { index, label },
                    ..
                } => vec![SemanticTag::new(NS_COMMON_NUMBER, *index).with_label(label)],
                Role::Child {
                    tags: ChildTags::Light { endpoint, label },
                    ..
                } => vec![
                    // A manufacturer tag carries the stable endpoint identity
                    // as (namespace high byte, tag low byte). Its label is
                    // mandatory and holds the saved member name.
                    SemanticTag {
                        mfg_code: Some(TEST_VID),
                        namespace_id: (*endpoint >> 8) as u8,
                        tag: *endpoint as u8,
                        label: Some(label),
                    },
                    SemanticTag::new(NS_COMMON_NUMBER, 0),
                ],
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
            matches!(ctx.cmd().cluster_id, 6 | 8 | 0x300)
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
