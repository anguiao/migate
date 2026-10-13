use super::{
    bridged_info::{BridgedHandler, GroupBridge},
    lighting,
    on_off::OnOffHandler,
    publication::{self, LightPublication, UnmappableReason},
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
        Async, AsyncHandler, ChainedHandler, Cluster, Dataver, DeviceType, EmptyHandler, Endpoint,
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
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
};

const AGGREGATOR_ENDPOINT: u16 = 1;
/// Vendor blob key for the canonical published-shape boundary. The first
/// vendor key is reserved for the migrated legacy label, so the shape uses the
/// next one. The value is MiGate-owned and private to this module.
const MATTER_SHAPE_KEY: u16 = rs_matter::persist::VENDOR_KEYS_START + 1;
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
    OnOffHandler::CLUSTER,
    BridgedHandler::CLUSTER
);
const LIGHT_IDENTIFY: rs_matter::dm::Cluster<'static> = identify::IdentifyHandler::<()>::CLUSTER
    .with_cmds(rs_matter::with!(identify::CommandId::Identify));
const PARENT_TYPES: &[DeviceType] = devices!(DEV_TYPE_BRIDGED_NODE);
const PARENT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] =
    clusters!(desc::DescHandler::CLUSTER, BridgedHandler::CLUSTER);
const CHILD_TYPES: &[DeviceType] = devices!(ON_OFF_PLUG);
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
    types: Box<[DeviceType]>,
    clusters: Box<[Cluster<'static>]>,
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
    },
}
struct Control {
    groups: Dataver,
    identify: identify::IdentifyHandler,
    on_off: Option<OnOffHandler>,
    lighting: Option<lighting::Lighting>,
}
impl Control {
    fn new(
        function: Function,
        endpoint: u16,
        versions: Versions,
        mapping: Option<LightPublication>,
    ) -> Self {
        let is_light = function.description().category == Category::Light;
        let mapping = is_light.then(|| mapping.expect("light function has a publication"));
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            on_off: (!is_light).then(|| {
                OnOffHandler::new(function.clone(), endpoint, Dataver::new(versions.on_off))
            }),
            lighting: mapping.map(|mapping| {
                lighting::Lighting::new(
                    function,
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                    mapping,
                )
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
            .chain(|_, cluster| cluster == 8, LevelDispatch(light))
            .chain(|_, cluster| cluster == 0x300, ColorDispatch(light))
    }
    fn on_off_version(&self) -> u32 {
        self.lighting.as_ref().map_or_else(
            || self.on_off.as_ref().unwrap().dataver(),
            |l| l.on_off_version.get(),
        )
    }
}
impl AsyncHandler for Control {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
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
            let publication = light.publication();
            let commands = match ctx.cmd().cluster_id {
                3 => LIGHT_IDENTIFY,
                4 => groups::GroupsHandler::CLUSTER,
                6 => lighting::OnOffHandler::CLUSTER,
                8 if publication.brightness => {
                    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER
                }
                0x300 if publication.has_color_control() => color_cluster(publication),
                _ => return Err(ErrorCode::ClusterNotFound.into()),
            };
            if commands.command(ctx.cmd().cmd_id).is_none() {
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
}
/// Dispatch only the real brightness dimension; a light without brightness has
/// no LevelControl controller to run.
struct LevelDispatch<'a>(&'a lighting::Lighting);
impl AsyncHandler for LevelDispatch<'_> {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if !self.0.publication().brightness {
            return Err(ErrorCode::ClusterNotFound.into());
        }
        level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
            .read(ctx, reply)
            .await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        if !self.0.publication().brightness {
            return Err(ErrorCode::ClusterNotFound.into());
        }
        level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
            .write(ctx)
            .await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if !self.0.publication().brightness {
            return Err(ErrorCode::ClusterNotFound.into());
        }
        level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
            .invoke(ctx, reply)
            .await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        if !self.0.publication().brightness {
            return std::future::pending().await;
        }
        level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
            .run(ctx)
            .await
    }
}
struct ColorDispatch<'a>(&'a lighting::Lighting);
impl ColorDispatch<'_> {
    fn mapped(&self) -> LightPublication {
        self.0.publication()
    }
}
impl AsyncHandler for ColorDispatch<'_> {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let publication = self.mapped();
        if !publication.has_color_control() {
            return Err(ErrorCode::ClusterNotFound.into());
        }
        if color_cluster(publication)
            .attribute(ctx.attr().attr_id)
            .is_none()
        {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        match (publication.temperature.is_some(), publication.rgb) {
            (true, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, true>(self.0))
                    .read(ctx, reply)
                    .await
            }
            (true, false) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, false>(self.0))
                    .read(ctx, reply)
                    .await
            }
            (false, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false, true>(self.0))
                    .read(ctx, reply)
                    .await
            }
            (false, false) => Err(ErrorCode::ClusterNotFound.into()),
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let publication = self.mapped();
        match (publication.temperature.is_some(), publication.rgb) {
            (true, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, true>(self.0))
                    .write(ctx)
                    .await
            }
            (true, false) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, false>(self.0))
                    .write(ctx)
                    .await
            }
            (false, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false, true>(self.0))
                    .write(ctx)
                    .await
            }
            (false, false) => Err(ErrorCode::ClusterNotFound.into()),
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let publication = self.mapped();
        match (publication.temperature.is_some(), publication.rgb) {
            (true, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, true>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            (true, false) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, false>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            (false, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false, true>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            (false, false) => Err(ErrorCode::ClusterNotFound.into()),
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let publication = self.mapped();
        match (publication.temperature.is_some(), publication.rgb) {
            (true, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, true>(self.0))
                    .run(ctx)
                    .await
            }
            (true, false) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, false>(self.0))
                    .run(ctx)
                    .await
            }
            (false, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false, true>(self.0))
                    .run(ctx)
                    .await
            }
            (false, false) => std::future::pending().await,
        }
    }
}
fn color_cluster(publication: LightPublication) -> Cluster<'static> {
    match (publication.temperature.is_some(), publication.rgb) {
        (true, true) => {
            <lighting::ColorHandler<true, true> as color_control::ClusterAsyncHandler>::CLUSTER
        }
        (true, false) => {
            <lighting::ColorHandler<true, false> as color_control::ClusterAsyncHandler>::CLUSTER
        }
        (false, true) => {
            <lighting::ColorHandler<false, true> as color_control::ClusterAsyncHandler>::CLUSTER
        }
        (false, false) => {
            <lighting::ColorHandler<false, false> as color_control::ClusterAsyncHandler>::CLUSTER
        }
    }
}
fn light_types(publication: LightPublication, single: bool) -> Box<[DeviceType]> {
    let mut types = vec![publication.device_type.matter_device_type()];
    if single {
        types.push(DEV_TYPE_BRIDGED_NODE);
    }
    types.into_boxed_slice()
}
fn light_clusters(publication: LightPublication, single: bool) -> Box<[Cluster<'static>]> {
    let mut clusters = vec![
        if single {
            desc::DescHandler::CLUSTER
        } else {
            desc::CLUSTER_TAG_LIST
        },
        groups::GroupsHandler::CLUSTER,
        LIGHT_IDENTIFY,
    ];
    clusters.push(lighting::OnOffHandler::CLUSTER);
    if publication.brightness {
        clusters.push(<lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER);
    }
    if publication.has_color_control() {
        clusters.push(color_cluster(publication));
    }
    // Bridge metadata follows the control clusters in the published server list.
    if single {
        clusters.push(BridgedHandler::CLUSTER);
    }
    clusters.into_boxed_slice()
}
fn static_types(types: &[DeviceType]) -> Box<[DeviceType]> {
    types.to_vec().into_boxed_slice()
}
fn static_clusters(clusters: &[Cluster<'static>]) -> Box<[Cluster<'static>]> {
    clusters.to_vec().into_boxed_slice()
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
            Self::Child { .. } => 0,
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
                    control.as_ref(),
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
        let endpoint = Endpoint::new(self.endpoint, &self.types, &self.clusters);
        match &self.role {
            Role::Child { .. } => endpoint.with_tags(tags),
            _ => endpoint,
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
            let publication = light.publication();
            if publication.brightness && cluster.is_none_or(|id| id == 8) {
                light.level_version.changed();
            }
            if publication.has_color_control() && cluster.is_none_or(|id| id == 0x300) {
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

struct Prepared {
    entries: Vec<Rc<Entry>>,
    member_updates: Vec<(Rc<GroupBridge>, Vec<PhysicalDevice>)>,
}

#[derive(Default)]
struct Reconcile {
    /// A replacement instance needs fresh state reports without changing its shape.
    state_refresh: Vec<u16>,
    controls: Vec<(u16, Option<Rc<Control>>)>,
    bridges: Vec<(u16, Option<Rc<BridgedHandler>>)>,
}

type JobResult = Result<Result<(), Error>, futures_util::future::Aborted>;
type JobFutures<'a> = FuturesUnordered<LocalBoxFuture<'a, JobResult>>;

pub(super) struct Model {
    registry: DeviceRegistry,
    store: Store,
    protocol: StoreAdapter,
    root: RootHandler,
    rebuild: Cell<bool>,
    entries: RefCell<Vec<Rc<Entry>>>,
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
        rebuild: Cell::new(false),
    };
    let prepared = model.prepare(&registry.snapshot())?;
    *model.entries.borrow_mut() = prepared.entries;
    Ok(model)
}

fn same_instance(entry: &Entry, device: &PhysicalDevice) -> bool {
    entry
        .device
        .as_ref()
        .is_some_and(|present| present.same_instance(device))
}
fn same_rc<T>(left: &Option<Rc<T>>, right: &Option<Rc<T>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Rc::ptr_eq(left, right),
        _ => false,
    }
}
fn encode_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn encode_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn encode_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}
fn encode_str(out: &mut Vec<u8>, value: &str) {
    encode_len(out, value.len());
    out.extend_from_slice(value.as_bytes());
}
fn encode_tag(
    out: &mut Vec<u8>,
    mfg_code: Option<u16>,
    namespace: u8,
    tag: u8,
    label: Option<&str>,
) {
    match mfg_code {
        Some(mfg_code) => {
            out.push(1);
            encode_u16(out, mfg_code);
        }
        None => out.push(0),
    }
    out.push(namespace);
    out.push(tag);
    match label {
        Some(label) => {
            out.push(1);
            encode_str(out, label);
        }
        None => out.push(0),
    }
}
/// Encode one advertised cluster exactly as the Descriptor and subscription
/// surfaces see it: id, features, revision and the real attribute/command IDs.
fn encode_cluster(out: &mut Vec<u8>, cluster: &Cluster<'_>) {
    encode_u32(out, cluster.id);
    encode_u32(out, cluster.feature_map);
    encode_u16(out, cluster.revision);
    let mut attributes = cluster
        .attributes
        .iter()
        .filter(|attribute| cluster.attribute(attribute.id).is_some())
        .map(|attribute| attribute.id)
        .collect::<Vec<_>>();
    attributes.sort_unstable();
    encode_len(out, attributes.len());
    for attribute in attributes {
        encode_u32(out, attribute);
    }
    let mut commands = cluster
        .commands
        .iter()
        .filter(|command| cluster.command(command.id).is_some())
        .map(|command| command.id)
        .collect::<Vec<_>>();
    commands.sort_unstable();
    encode_len(out, commands.len());
    for command in commands {
        encode_u32(out, command);
    }
}
fn encode_metadata(out: &mut Vec<u8>, id: u16, types: &[DeviceType], clusters: &[Cluster<'_>]) {
    encode_u16(out, id);
    let mut types = types
        .iter()
        .map(|device_type| (device_type.dtype, device_type.drev))
        .collect::<Vec<_>>();
    types.sort_unstable();
    encode_len(out, types.len());
    for (dtype, drev) in types {
        encode_u16(out, dtype);
        encode_u16(out, drev);
    }
    let mut clusters = clusters.iter().collect::<Vec<_>>();
    clusters.sort_by_key(|cluster| cluster.id);
    encode_len(out, clusters.len());
    for cluster in clusters {
        encode_cluster(out, cluster);
    }
}
fn encode_light(out: &mut Vec<u8>, control: &Control) {
    match control.lighting.as_ref().map(|light| light.publication()) {
        None => out.push(0),
        Some(publication) => {
            out.push(1);
            out.push(u8::from(publication.brightness));
            out.push(u8::from(publication.rgb));
            match publication.temperature {
                Some(temperature) => {
                    out.push(1);
                    encode_u16(out, temperature.bounds.0);
                    encode_u16(out, temperature.bounds.1);
                }
                None => out.push(0),
            }
        }
    }
}
/// Canonical per-endpoint shape. Order-insensitive collections are sorted so a
/// pure reorder never looks like a structural change; real attribute/command,
/// feature, revision, device-type and CT-bound changes do.
fn encode_entry(out: &mut Vec<u8>, entry: &Entry) {
    encode_metadata(out, entry.endpoint, &entry.types, &entry.clusters);
    out.push(u8::from(entry.role.bridged_rc().is_some()));
    match &entry.role {
        Role::Single { control, .. } => {
            out.push(1);
            encode_len(out, 0);
            encode_len(out, 0);
            encode_light(out, control);
            out.push(0);
        }
        Role::Parent { children, bridged } => {
            out.push(2);
            let mut children = children.clone();
            children.sort_unstable();
            encode_len(out, children.len());
            for child in children {
                encode_u16(out, child);
            }
            encode_len(out, 0);
            out.push(0);
            match bridged.group_key() {
                Some(key) => {
                    out.push(1);
                    encode_str(out, &key.source);
                    encode_str(out, &key.id);
                }
                None => out.push(0),
            }
        }
        Role::Child { control, tags, .. } => {
            out.push(3);
            encode_len(out, 0);
            match tags {
                ChildTags::Outlet { index, label } => {
                    encode_len(out, 1);
                    encode_tag(out, None, NS_COMMON_NUMBER, *index, Some(label));
                }
                ChildTags::Light { endpoint, label } => {
                    encode_len(out, 2);
                    encode_tag(
                        out,
                        Some(TEST_VID),
                        (*endpoint >> 8) as u8,
                        *endpoint as u8,
                        Some(label),
                    );
                    encode_tag(out, None, NS_COMMON_NUMBER, 0, None);
                }
            }
            encode_light(out, control);
            out.push(0);
        }
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
    /// Canonical bytes of the whole published surface, including the static
    /// root and aggregator metadata so protocol or app upgrades that change
    /// them are detected too. Dynamic NodeLabel and source values are excluded.
    fn shape_bytes(&self) -> Vec<u8> {
        Self::encode_shape(&self.entries.borrow())
    }
    fn encode_shape(entries: &[Rc<Entry>]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_metadata(&mut out, ROOT.id, ROOT.device_types, ROOT.clusters);
        encode_metadata(
            &mut out,
            AGGREGATOR.id,
            AGGREGATOR.device_types,
            AGGREGATOR.clusters,
        );
        for entry in entries {
            encode_entry(&mut out, entry);
        }
        out
    }
    fn prepare(&self, snapshot: &RegistrySnapshot) -> Result<Prepared, StorageError> {
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
        let mut light_publication =
            HashMap::<DeviceKey, Result<LightPublication, UnmappableReason>>::new();
        for device in &snapshot.devices {
            let functions = device.functions();
            if functions.len() == 1 && functions[0].description().category == Category::Light {
                light_publication.insert(
                    device.key().clone(),
                    publication::map_light(functions[0].description()),
                );
            }
        }
        let member_publishable =
            |key: &DeviceKey| light_publication.get(key).is_some_and(Result::is_ok);
        let mut member_group = HashMap::<DeviceKey, DeviceKey>::new();
        for group in &snapshot.composition.groups {
            if group.members.iter().all(&member_publishable) {
                for member in &group.members {
                    member_group.insert(member.clone(), group.key.clone());
                }
            } else {
                log::info!(
                    "Matter group {} stays unpublished: a member is not Matter-publishable",
                    group.key.id
                );
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
                    types: static_types(PARENT_TYPES),
                    clusters: static_clusters(PARENT_CLUSTERS),
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
                                None,
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
                        },
                        types: static_types(CHILD_TYPES),
                        clusters: static_clusters(CHILD_CLUSTERS),
                    }));
                }
            } else {
                let function = &identity.functions[0];
                let endpoint = function.endpoint;
                let versions = self.versions_at(&previous_by_endpoint, endpoint);
                let inherited = previous_by_endpoint
                    .get(&endpoint)
                    .filter(|entry| same_instance(entry, device));
                let mapping = match light_publication.get(device.key()) {
                    Some(Ok(mapping)) => Some(*mapping),
                    Some(Err(reason)) => {
                        log::info!(
                            "Matter device {} stays unpublished: {}",
                            device.key().id,
                            reason.as_str()
                        );
                        continue;
                    }
                    None => None,
                };
                let control = inherited
                    .and_then(|entry| entry.role.control_rc())
                    .unwrap_or_else(|| {
                        Rc::new(Control::new(
                            device.function(&function.key.id).unwrap(),
                            endpoint,
                            versions,
                            mapping,
                        ))
                    });
                if member_group.contains_key(device.key()) {
                    let publication = mapping.expect("group members are Matter-publishable");
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
                        },
                        types: light_types(publication, false),
                        clusters: light_clusters(publication, false),
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
                    let (types, clusters) = match mapping {
                        Some(publication) => (
                            light_types(publication, true),
                            light_clusters(publication, true),
                        ),
                        None => (static_types(SINGLE_TYPES), static_clusters(SINGLE_CLUSTERS)),
                    };
                    desired.push(Rc::new(Entry {
                        endpoint,
                        device: Some(device.clone()),
                        descriptor: Dataver::new(versions.descriptor),
                        role: Role::Single { control, bridged },
                        types,
                        clusters,
                    }));
                }
            }
        }

        for group in &snapshot.composition.groups {
            if !group.members.iter().all(&member_publishable) {
                continue;
            }
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
                types: static_types(PARENT_TYPES),
                clusters: static_clusters(PARENT_CLUSTERS),
            }));
        }

        desired.sort_by_key(|entry| entry.endpoint);
        for pair in desired.windows(2) {
            if pair[0].endpoint == pair[1].endpoint {
                return Err(self.invalid("Reconciled endpoints are duplicated"));
            }
        }
        Ok(Prepared {
            entries: desired,
            member_updates,
        })
    }

    fn reconcile(&self, snapshot: &RegistrySnapshot) -> Result<Option<Reconcile>, StorageError> {
        let Prepared {
            entries,
            member_updates,
        } = self.prepare(snapshot)?;
        // Nothing shared with the current model is changed until this check.
        // A new service instance will publish the latest confirmed topology.
        if Self::encode_shape(&entries) != self.shape_bytes() {
            self.rebuild.set(true);
            return Ok(None);
        }
        let previous = self.entries.borrow();
        let mut report = Reconcile::default();
        for (entry, previous) in entries.iter().zip(previous.iter()) {
            if let (Some(old), Some(new)) = (&previous.device, &entry.device)
                && !old.same_instance(new)
            {
                report.state_refresh.push(entry.endpoint);
            }
            let control = entry.role.control_rc();
            if !same_rc(&control, &previous.role.control_rc()) {
                report.controls.push((entry.endpoint, control));
            }
            let bridged = entry.role.bridged_rc();
            if !same_rc(&bridged, &previous.role.bridged_rc()) {
                report.bridges.push((entry.endpoint, bridged));
            }
        }
        drop(previous);
        for (bridge, members) in member_updates {
            bridge.set_members(members);
        }
        *self.entries.borrow_mut() = entries;
        Ok(Some(report))
    }

    pub(super) fn needs_rebuild(&self) -> bool {
        self.rebuild.get()
    }

    /// Establish the complete persisted shape/version boundary before serving
    /// exchanges or resumed subscriptions. Repeating this is a no-op.
    pub(super) fn initialize(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let shape = self.shape_bytes();
        let stored = self
            .protocol
            .record(self.store.matter().get(MATTER_SHAPE_KEY))?;
        if stored.as_deref() != Some(shape.as_slice()) {
            if let Err(error) = ctx.matter().bump_configuration_version(ctx.kv(), &ctx) {
                self.protocol.record::<()>(Err(StorageError::new(
                    self.store.path(),
                    "persist Matter configuration version",
                    error,
                )))?;
            }
            self.protocol
                .record(self.store.matter().put(MATTER_SHAPE_KEY, &shape))?;
        }
        Ok(())
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
            async move { control.run(ctx).await },
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
    async fn job_failure(tasks: &mut JobFutures<'_>) -> Error {
        while let Some(result) = tasks.next().await {
            match result {
                Ok(Err(error)) => return error,
                Ok(Ok(())) => return ErrorCode::InvalidState.into(),
                Err(_) => {}
            }
        }
        std::future::pending().await
    }

    async fn run_devices(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.registry.subscribe();
        let mut control_jobs = BTreeMap::<u16, AbortHandle>::new();
        let mut bridge_jobs = BTreeMap::<u16, AbortHandle>::new();
        let mut tasks = JobFutures::new();
        loop {
            let Some(report) = self
                .protocol
                .record(self.reconcile(&self.registry.snapshot()))?
            else {
                return Ok(());
            };
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
            let mut cluster_notifications = BTreeMap::<u16, BTreeSet<u32>>::new();
            let mut reachable_refresh = BTreeSet::new();
            // A rejoined device starts from the current source snapshot; its
            // control clusters may now be unknown or reset. The published
            // surface itself is unchanged, so this is not a configuration
            // change and must not bump the configuration version.
            for endpoint in &report.state_refresh {
                let entry = self.entry(*endpoint)?;
                if let Some(control) = entry.role.control() {
                    let clusters = cluster_notifications.entry(*endpoint).or_default();
                    clusters.insert(6);
                    if let Some(light) = &control.lighting {
                        let publication = light.publication();
                        if publication.brightness {
                            clusters.insert(8);
                        }
                        if publication.has_color_control() {
                            clusters.insert(0x300);
                        }
                    }
                }
                // A device-backed bridge keeps its BDBI, but the replacement
                // seeds `reported` from the new instance's availability, so
                // its run loop stays silent. Report Reachable explicitly; the
                // retained group-parent aggregator keeps tracking its own
                // reported value.
                if entry
                    .role
                    .bridged_rc()
                    .is_some_and(|bridged| bridged.group_key().is_none())
                {
                    reachable_refresh.insert(*endpoint);
                }
            }
            for (endpoint, clusters) in cluster_notifications {
                for cluster in clusters {
                    ctx.notify_cluster_changed(endpoint, cluster);
                }
            }
            for endpoint in reachable_refresh {
                ctx.notify_attr_changed(endpoint, 0x39, 17);
            }
            futures_lite::future::or(
                async {
                    changes.changed().await;
                    Ok(())
                },
                async { Err(Self::job_failure(&mut tasks).await) },
            )
            .await?;
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
        self.initialize(&ctx)?;
        futures_lite::future::or(self.root.run(&ctx), self.run_devices(&ctx)).await
    }
}
