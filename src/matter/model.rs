use super::{
    bridged_info::{BridgedHandler, LIGHT_CLUSTER, Target as BridgeTarget},
    lighting,
    on_off::OnOffHandler,
    publication::{self, LightPublication},
    storage::StoreAdapter,
};
use crate::{
    device::{
        Capability, Category, DeviceKey, DeviceRegistry, Function, Group, Light, PhysicalDevice,
    },
    lighting::LightingExecutor,
    storage::{
        BlobChange, CompositionCommitError, ConfigurationVersion, Identity, StorageError, Store,
        StoredDevice,
    },
};
use futures_util::{StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use rand::RngExt as _;
use rs_matter::{
    Matter, clusters, devices,
    dm::{
        Async, AsyncHandler, ChainedHandler, Cluster, Dataver, DeviceType, EmptyHandler, Endpoint,
        FnMatcher, Handler, HandlerContext, InvokeContext, InvokeReply, LifecycleOp, MatchContext,
        Metadata, Node, ReadContext, ReadReply, SemanticTag, WriteContext,
        clusters::{
            app::on_off::{self, ClusterAsyncHandler as _},
            basic_info::{self, BasicInfoConfig, BasicInfoSettings},
            decl::{
                bridged_device_basic_information::{self as bridged, ClusterHandler as _},
                color_control, level_control,
            },
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
    tlv::{FromTLV, TLVElement, TLVTag, ToTLV},
    utils::{storage::WriteBuf, sync::DynBase},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};

const AGGREGATOR_ENDPOINT: u16 = 1;
/// MiGate-owned blob key for the canonical published-shape boundary.
const MATTER_SHAPE_KEY: u16 = rs_matter::persist::VENDOR_KEYS_START + 1;
/// Shape-blob framing marker. The payload is a count followed by
/// length-prefixed per-endpoint sections, so the startup baseline can diff
/// individual endpoints without parsing the fixed metadata encoding.
const SHAPE_MAGIC: &[u8; 4] = b"MG6\0";
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const BDBI_CLUSTER: u32 = bridged::FULL_CLUSTER.id;
const BDBI_CONFIGURATION_VERSION: u32 = bridged::AttributeId::ConfigurationVersion as u32;
const BASIC_INFO_CLUSTER: u32 = basic_info::FULL_CLUSTER.id;
const BASIC_INFO_CONFIGURATION_VERSION: u32 = basic_info::AttributeId::ConfigurationVersion as u32;
const DESCRIPTOR_CLUSTER: u32 = desc::DescHandler::CLUSTER.id;
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
        let mut settings = BasicInfoSettings::new();
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
    descriptor: Dataver,
    role: Role,
    shape: ShapeEntry,
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
        index: u8,
        label: String,
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
        executor: Rc<LightingExecutor>,
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
                    Light::Single(function),
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                    mapping,
                    executor,
                )
            }),
        }
    }

    /// The virtual group entry. It shares the registry's applied [`Group`]
    /// handle with commands, aggregation and the terminal; nothing here owns
    /// a second member set.
    fn new_group(
        group: Group,
        endpoint: u16,
        versions: Versions,
        mapping: LightPublication,
        executor: Rc<LightingExecutor>,
    ) -> Self {
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            on_off: None,
            lighting: Some(lighting::Lighting::new(
                Light::Group(group),
                endpoint,
                Dataver::new(versions.on_off),
                Dataver::new(versions.level),
                Dataver::new(versions.color),
                mapping,
                executor,
            )),
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
}
impl AsyncHandler for Control {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the cluster versions. */
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
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the cluster versions. */
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
macro_rules! dispatch_color {
    ($light:expr, $method:ident($($arg:expr),*), $missing:expr) => {
        match ($light.publication().temperature.is_some(), $light.publication().rgb) {
            (true, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, true>($light))
                    .$method($($arg),*).await
            }
            (true, false) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true, false>($light))
                    .$method($($arg),*).await
            }
            (false, true) => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false, true>($light))
                    .$method($($arg),*).await
            }
            (false, false) => $missing,
        }
    };
}
impl AsyncHandler for ColorDispatch<'_> {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let publication = self.0.publication();
        if !publication.has_color_control() {
            return Err(ErrorCode::ClusterNotFound.into());
        }
        if color_cluster(publication)
            .attribute(ctx.attr().attr_id)
            .is_none()
        {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        dispatch_color!(
            self.0,
            read(ctx, reply),
            Err(ErrorCode::ClusterNotFound.into())
        )
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        dispatch_color!(self.0, write(ctx), Err(ErrorCode::ClusterNotFound.into()))
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        dispatch_color!(
            self.0,
            invoke(ctx, reply),
            Err(ErrorCode::ClusterNotFound.into())
        )
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        dispatch_color!(self.0, run(ctx), std::future::pending().await)
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
fn light_types(publication: LightPublication) -> Vec<DeviceType> {
    vec![
        publication.device_type.matter_device_type(),
        DEV_TYPE_BRIDGED_NODE,
    ]
}
fn light_clusters(publication: LightPublication) -> Vec<Cluster<'static>> {
    let mut clusters = vec![
        desc::DescHandler::CLUSTER,
        groups::GroupsHandler::CLUSTER,
        LIGHT_IDENTIFY,
        lighting::OnOffHandler::CLUSTER,
    ];
    if publication.brightness {
        clusters.push(<lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER);
    }
    if publication.has_color_control() {
        clusters.push(color_cluster(publication));
    }
    // Bridge metadata follows the control clusters in the published server list.
    clusters.push(LIGHT_CLUSTER);
    clusters
}

/// One endpoint of the canonical fixed-shape encoding. It deliberately does
/// not own handlers or shared state, so the whole candidate surface can be
/// built, compared and encoded before any storage transaction.
#[derive(Clone)]
struct ShapeEntry {
    endpoint: u16,
    types: Vec<DeviceType>,
    clusters: Vec<Cluster<'static>>,
    role: ShapeRole,
}
#[derive(Clone)]
enum ShapeRole {
    Single {
        bridged: bool,
        light: Option<LightPublication>,
        group: Option<DeviceKey>,
    },
    Parent {
        children: Vec<u16>,
    },
    Child {
        index: u8,
        label: String,
    },
}

impl ShapeEntry {
    fn advertises_configuration_version(&self) -> bool {
        self.clusters.iter().any(|cluster| {
            cluster.id == BDBI_CLUSTER
                && cluster
                    .attribute(bridged::AttributeId::ConfigurationVersion as _)
                    .is_some()
        })
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
                    control,
                ), $($arg),*
            ).await,
            Role::Parent { bridged, .. } => AsyncHandler::$method(
                &EmptyHandler.chain(
                    |_, cluster| cluster == 0x39,
                    Async(bridged::HandlerAdaptor(bridged)),
                ), $($arg),*
            ).await,
            Role::Child { control, .. } => control.$method($($arg),*).await,
        }
    };
}

impl Entry {
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        let endpoint = Endpoint::new(self.endpoint, &self.shape.types, &self.shape.clusters);
        match &self.role {
            Role::Child { .. } => endpoint.with_tags(tags),
            _ => endpoint,
        }
    }
    fn configuration_version(&self) -> u32 {
        self.role
            .bridged()
            .map(|bridged| bridged.configuration_version_value())
            .unwrap_or(1)
    }
    fn shape(&self) -> ShapeEntry {
        self.shape.clone()
    }

    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        if ctx.attr().cluster_id == DESCRIPTOR_CLUSTER {
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
        if cluster.is_none_or(|id| id == DESCRIPTOR_CLUSTER) {
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
            && cluster.is_none_or(|id| id == BDBI_CLUSTER)
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

/// A fully resolved publication decision. Every fallible step (identity
/// lookups, member binding, capability mapping, TLV encoding) has already
/// happened before this is built; applying it can only construct handlers.
enum DesiredKind {
    Device {
        device: PhysicalDevice,
        function: Function,
        stored: StoredDevice,
        mapping: Option<LightPublication>,
    },
    Parent {
        device: PhysicalDevice,
        stored: StoredDevice,
        children: Vec<u16>,
    },
    Child {
        function: Function,
        index: u8,
        label: String,
    },
    Group {
        key: DeviceKey,
        identity: String,
        mapping: LightPublication,
    },
}
struct DesiredEntry {
    endpoint: u16,
    kind: DesiredKind,
    configuration_version: u32,
}
impl DesiredEntry {
    fn advertises_configuration_version(&self) -> bool {
        match &self.kind {
            DesiredKind::Group { .. } => true,
            DesiredKind::Device { mapping, .. } => mapping.is_some(),
            DesiredKind::Parent { .. } | DesiredKind::Child { .. } => false,
        }
    }
    fn shape(&self) -> ShapeEntry {
        let (role, types, clusters) = match &self.kind {
            DesiredKind::Device { mapping, .. } => match mapping {
                Some(publication) => (
                    ShapeRole::Single {
                        bridged: true,
                        light: Some(*publication),
                        group: None,
                    },
                    light_types(*publication),
                    light_clusters(*publication),
                ),
                None => (
                    ShapeRole::Single {
                        bridged: true,
                        light: None,
                        group: None,
                    },
                    SINGLE_TYPES.to_vec(),
                    SINGLE_CLUSTERS.to_vec(),
                ),
            },
            DesiredKind::Parent { children, .. } => (
                ShapeRole::Parent {
                    children: children.clone(),
                },
                PARENT_TYPES.to_vec(),
                PARENT_CLUSTERS.to_vec(),
            ),
            DesiredKind::Child { index, label, .. } => (
                ShapeRole::Child {
                    index: *index,
                    label: label.clone(),
                },
                CHILD_TYPES.to_vec(),
                CHILD_CLUSTERS.to_vec(),
            ),
            DesiredKind::Group { key, mapping, .. } => (
                ShapeRole::Single {
                    bridged: true,
                    light: Some(*mapping),
                    group: Some(key.clone()),
                },
                light_types(*mapping),
                light_clusters(*mapping),
            ),
        };
        ShapeEntry {
            endpoint: self.endpoint,
            types,
            clusters,
            role,
        }
    }
}

/// Fixed-description changes shared by startup and online configuration.
/// The versions and protocol blobs are committed at the same boundary.
struct ShapeChanges {
    versions: Vec<ConfigurationVersion>,
    blobs: Vec<BlobChange>,
    settings: Option<BasicInfoSettings>,
}

/// Requests a new Matter instance. The latest candidate is resolved only
/// after the old instance and all its exchanges have been dropped.
pub(super) struct Rebuild {
    registry: DeviceRegistry,
    store: Store,
    retry: Option<Retry>,
}
impl Rebuild {
    pub(super) fn apply(self) -> Option<Retry> {
        let result = self
            .store
            .devices()
            .apply_pending_configuration(&self.registry, |prepared| {
                let groups = prepared
                    .groups()
                    .iter()
                    .map(|group| {
                        (
                            &group.key,
                            group.endpoint,
                            group.unique_id.clone(),
                            group.capabilities.as_slice(),
                        )
                    })
                    .collect();
                let desired =
                    desired_configuration(&self.store, groups, &prepared.pending().devices)?;
                let shapes = desired
                    .iter()
                    .map(|entry| (entry.shape(), entry.configuration_version))
                    .collect::<Vec<_>>();
                let previous = self.store.matter().get(MATTER_SHAPE_KEY)?;
                let settings = self
                    .store
                    .matter()
                    .get(rs_matter::persist::BASIC_INFO_KEY)?
                    .ok_or_else(|| {
                        invalid(
                            &self.store,
                            "running Matter instance has no saved basic information",
                        )
                    })?;
                let settings =
                    BasicInfoSettings::from_tlv(&TLVElement::new(&settings)).map_err(|error| {
                        StorageError::new(self.store.path(), "restore Matter configuration", error)
                    })?;
                let changes = shape_changes(&self.store, settings, previous.as_deref(), &shapes)?;
                Ok((changes.versions, changes.blobs))
            });
        match result {
            Ok(confirmed) => {
                self.registry.clear_apply_error();
                if let Some(confirmed) = confirmed {
                    log::info!(
                        "Matter configuration applied: version {}",
                        confirmed.version
                    );
                }
                None
            }
            Err(error) => Retry::failed(&self.registry, self.retry, &error),
        }
    }
}

type JobFutures<'a> = FuturesUnordered<LocalBoxFuture<'a, Result<(), Error>>>;
pub(super) struct Retry {
    deadline: Instant,
    backoff: Duration,
    revision: u64,
    message: String,
}
impl Retry {
    fn failed(
        registry: &DeviceRegistry,
        previous: Option<Self>,
        failure: &impl std::fmt::Display,
    ) -> Option<Self> {
        let message = failure.to_string();
        let snapshot = registry.snapshot();
        let backoff = match &previous {
            Some(state) if state.revision == snapshot.revision => {
                (state.backoff * 2).min(MAX_RETRY_DELAY)
            }
            _ => RETRY_DELAY,
        };
        if previous
            .as_ref()
            .is_none_or(|state| state.message != message)
        {
            log::warn!(
                "Matter configuration apply failed: {message}; keeping the last applied configuration and retrying"
            );
        } else {
            log::debug!("Matter configuration apply still failing: {message}");
        }
        registry.set_apply_error(message.clone());
        snapshot.pending.map(|_| Self {
            deadline: Instant::now() + backoff,
            backoff,
            revision: snapshot.revision,
            message,
        })
    }
}

pub(super) struct Model {
    registry: DeviceRegistry,
    store: Store,
    protocol: StoreAdapter,
    executor: Rc<LightingExecutor>,
    root: RootHandler,
    entries: Vec<Entry>,
    rebuild: RefCell<Option<Rebuild>>,
    retry: RefCell<Option<Retry>>,
}

impl Drop for Model {
    fn drop(&mut self) {
        self.executor.reset();
    }
}

pub(super) fn handler(
    registry: &DeviceRegistry,
    store: &Store,
    protocol: StoreAdapter,
    executor: Rc<LightingExecutor>,
    mut random: impl rand::Rng,
    retry: Option<Retry>,
) -> Result<Model, StorageError> {
    let mut model = Model {
        registry: registry.clone(),
        store: store.clone(),
        protocol,
        executor,
        root: endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(&mut random)
            .chain(
                |endpoint, cluster| {
                    endpoint == AGGREGATOR_ENDPOINT && cluster == DESCRIPTOR_CLUSTER
                },
                Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
            ),
        entries: Vec::new(),
        rebuild: RefCell::new(None),
        retry: RefCell::new(retry),
    };
    model.entries = model.initial_entries()?;
    Ok(model)
}
fn commit_error(
    store: &Store,
    operation: &'static str,
    error: CompositionCommitError,
) -> StorageError {
    match error {
        CompositionCommitError::Storage(error) => error,
        error => StorageError::new(store.path(), operation, error),
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
fn encode_publication(out: &mut Vec<u8>, publication: LightPublication) {
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
fn encode_group(out: &mut Vec<u8>, group: &Option<DeviceKey>) {
    match group {
        Some(key) => {
            out.push(1);
            encode_str(out, &key.source);
            encode_str(out, &key.id);
        }
        None => out.push(0),
    }
}
/// Canonical per-endpoint shape. Order-insensitive collections are sorted so a
/// pure reorder never looks like a structural change; real attribute/command,
/// feature, revision, device-type, range and role changes do.
fn encode_entry(out: &mut Vec<u8>, entry: &ShapeEntry) {
    encode_metadata(out, entry.endpoint, &entry.types, &entry.clusters);
    match &entry.role {
        ShapeRole::Single {
            bridged,
            light,
            group,
        } => {
            out.push(1);
            out.push(u8::from(*bridged));
            match light {
                Some(publication) => {
                    out.push(1);
                    encode_publication(out, *publication);
                }
                None => out.push(0),
            }
            encode_group(out, group);
        }
        ShapeRole::Parent { children } => {
            out.push(2);
            let mut children = children.clone();
            children.sort_unstable();
            encode_len(out, children.len());
            for child in children {
                encode_u16(out, child);
            }
            out.push(0);
            out.push(0);
        }
        ShapeRole::Child { index, label } => {
            out.push(3);
            encode_len(out, 1);
            encode_tag(out, None, NS_COMMON_NUMBER, *index, Some(label));
            out.push(0);
            out.push(0);
        }
    }
}
fn encode_section(entry: &ShapeEntry) -> Vec<u8> {
    let mut section = Vec::new();
    encode_entry(&mut section, entry);
    section
}
fn root_shape() -> ShapeEntry {
    ShapeEntry {
        endpoint: ROOT.id,
        types: ROOT.device_types.to_vec(),
        clusters: ROOT.clusters.to_vec(),
        role: ShapeRole::Single {
            bridged: false,
            light: None,
            group: None,
        },
    }
}
fn aggregator_shape() -> ShapeEntry {
    ShapeEntry {
        endpoint: AGGREGATOR.id,
        types: AGGREGATOR.device_types.to_vec(),
        clusters: AGGREGATOR.clusters.to_vec(),
        role: ShapeRole::Single {
            bridged: false,
            light: None,
            group: None,
        },
    }
}
fn encode_shape(entries: &[ShapeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SHAPE_MAGIC);
    encode_len(&mut out, entries.len() + 2);
    for entry in [root_shape(), aggregator_shape()].iter().chain(entries) {
        let section = encode_section(entry);
        encode_len(&mut out, section.len());
        out.extend_from_slice(&section);
    }
    out
}
fn decode_len(rest: &mut &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = rest.get(..8)?.try_into().ok()?;
    *rest = &rest[8..];
    Some(u64::from_le_bytes(bytes))
}
/// Split a stored shape blob into per-endpoint section bytes. `None` when the
/// blob came from a different framing (or is corrupted); the caller then
/// treats the whole surface as changed.
fn decode_shape(bytes: &[u8]) -> Option<BTreeMap<u16, Vec<u8>>> {
    let mut rest = bytes.strip_prefix(SHAPE_MAGIC)?;
    let count = decode_len(&mut rest)?;
    let mut sections = BTreeMap::new();
    for _ in 0..count {
        let len = usize::try_from(decode_len(&mut rest)?).ok()?;
        let section = rest.get(..len)?;
        rest = &rest[len..];
        let endpoint = u16::from_le_bytes([*section.first()?, *section.get(1)?]);
        if sections.insert(endpoint, section.to_vec()).is_some() {
            return None;
        }
    }
    Some(sections)
}
fn encode_basic_info(settings: &BasicInfoSettings) -> Result<Vec<u8>, Error> {
    let mut buffer = vec![0u8; 512];
    let mut write = WriteBuf::new(&mut buffer);
    settings.to_tlv(&TLVTag::Anonymous, &mut write)?;
    Ok(write.as_slice().to_vec())
}

impl Model {
    fn entry(&self, endpoint: u16) -> Result<&Entry, Error> {
        self.entries
            .iter()
            .find(|entry| entry.endpoint == endpoint)
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
    }

    /// Build every published endpoint from the registry's applied
    /// configuration. Startup uses the last committed identities, endpoints
    /// and derived capabilities; a staged candidate is not mixed in.
    fn initial_entries(&self) -> Result<Vec<Entry>, StorageError> {
        let applied = self.registry.snapshot().applied;
        let descriptions = applied
            .groups
            .iter()
            .map(|group| group.description())
            .collect::<Vec<_>>();
        let groups = applied
            .groups
            .iter()
            .zip(&descriptions)
            .map(|(group, description)| {
                let identity = self
                    .store
                    .devices()
                    .load_group_identity(&group.key)?
                    .ok_or_else(|| invalid(&self.store, "applied group has no stored identity"))?;
                Ok((
                    &group.key,
                    identity.endpoint,
                    identity.unique_id,
                    description.capabilities.as_slice(),
                ))
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        let desired = desired_configuration(&self.store, groups, &applied.devices)?;
        Ok(desired
            .iter()
            .map(|desired| self.build_entry(desired, &applied))
            .collect())
    }
}

/// Applied and pending configurations supply the same resolved sources and
/// reserved group identities to a single publication mapper.
fn desired_configuration(
    store: &Store,
    groups: Vec<(&DeviceKey, u16, String, &[Capability])>,
    devices: &[PhysicalDevice],
) -> Result<Vec<DesiredEntry>, StorageError> {
    let device_store = store.devices();
    let mut desired = Vec::new();
    for (key, endpoint, identity, capabilities) in groups {
        let mapping = publication::map_capabilities(capabilities)
            .map_err(|reason| invalid(store, reason.as_str()))?;
        desired.push(DesiredEntry {
            endpoint,
            kind: DesiredKind::Group {
                key: key.clone(),
                identity,
                mapping,
            },
            configuration_version: 1,
        });
    }
    for device in devices {
        let stored = device_store
            .load_published(device.key())?
            .ok_or_else(|| invalid(store, "standalone endpoint has no published identity"))?;
        desired.extend(desired_device_entries(store, device, &stored)?);
    }
    for entry in &mut desired {
        if entry.advertises_configuration_version() {
            entry.configuration_version = device_store
                .configuration_version(entry.endpoint)?
                .unwrap_or(1);
        }
    }
    desired.sort_by_key(|entry| entry.endpoint);
    Ok(desired)
}

/// Resolve one physical device into its published entries. A multi-channel
/// device keeps the outlet parent/child structure; a single function is one
/// bridged endpoint (a light when its description maps, an outlet
/// otherwise). An unmappable light is not published at all.
fn desired_device_entries(
    store: &Store,
    device: &PhysicalDevice,
    stored: &StoredDevice,
) -> Result<Vec<DesiredEntry>, StorageError> {
    let mut desired = Vec::new();
    if stored.functions.len() > 1 {
        desired.push(DesiredEntry {
            endpoint: stored.endpoint,
            kind: DesiredKind::Parent {
                device: device.clone(),
                stored: stored.clone(),
                children: stored
                    .functions
                    .iter()
                    .map(|function| function.endpoint)
                    .collect(),
            },
            configuration_version: 1,
        });
        for (index, function) in stored.functions.iter().enumerate() {
            let Some(live) = device.function(&function.key.id) else {
                return Err(invalid(
                    store,
                    "published device function is missing at runtime",
                ));
            };
            desired.push(DesiredEntry {
                endpoint: function.endpoint,
                kind: DesiredKind::Child {
                    function: live,
                    index: index as u8 + 1,
                    label: function.label.clone(),
                },
                configuration_version: 1,
            });
        }
        return Ok(desired);
    }
    let Some(function) = stored.functions.first() else {
        return Err(invalid(store, "published device has no functions"));
    };
    let Some(live) = device.function(&function.key.id) else {
        return Err(invalid(
            store,
            "published device function is missing at runtime",
        ));
    };
    let mapping = if function.category == Category::Light {
        match publication::map_light(live.description()) {
            Ok(mapping) => Some(mapping),
            Err(reason) => {
                log::info!(
                    "Matter device {} stays unpublished: {}",
                    device.key().id,
                    reason.as_str()
                );
                return Ok(Vec::new());
            }
        }
    } else {
        None
    };
    desired.push(DesiredEntry {
        endpoint: function.endpoint,
        kind: DesiredKind::Device {
            device: device.clone(),
            function: live,
            stored: stored.clone(),
            mapping,
        },
        configuration_version: 1,
    });
    Ok(desired)
}

/// Compare only fixed descriptions. A decoded baseline that lacks an
/// endpoint represents a new publication; its reserved version advances
/// when that identity has already been published. A missing or legacy
/// baseline retains stored endpoint versions while establishing the shape.
fn shape_changes(
    store: &Store,
    mut settings: BasicInfoSettings,
    previous: Option<&[u8]>,
    entries: &[(ShapeEntry, u32)],
) -> Result<ShapeChanges, StorageError> {
    let shape = encode_shape(
        &entries
            .iter()
            .map(|(shape, _)| shape.clone())
            .collect::<Vec<_>>(),
    );
    if previous == Some(shape.as_slice()) {
        return Ok(ShapeChanges {
            versions: Vec::new(),
            blobs: Vec::new(),
            settings: None,
        });
    }
    let sections = previous.and_then(decode_shape);
    let mut versions = Vec::new();
    for (entry, current) in entries {
        if !entry.advertises_configuration_version() {
            continue;
        }
        let stored = store.devices().configuration_version(entry.endpoint)?;
        let old = sections
            .as_ref()
            .and_then(|sections| sections.get(&entry.endpoint));
        let version = match old {
            Some(old) if *old != encode_section(entry) => {
                (*current).max(stored.unwrap_or(1)).saturating_add(1)
            }
            Some(_) => stored.unwrap_or(*current),
            None if sections.is_some() => stored.map_or(1, |version| version.saturating_add(1)),
            None => stored.unwrap_or(1),
        };
        versions.push(ConfigurationVersion {
            endpoint: entry.endpoint,
            version,
        });
    }
    settings.bump_configuration_version();
    let bytes = encode_basic_info(&settings)
        .map_err(|error| StorageError::new(store.path(), "encode Matter configuration", error))?;
    Ok(ShapeChanges {
        versions,
        blobs: vec![
            BlobChange {
                key: rs_matter::persist::BASIC_INFO_KEY,
                value: Some(bytes),
            },
            BlobChange {
                key: MATTER_SHAPE_KEY,
                value: Some(shape),
            },
        ],
        settings: Some(settings),
    })
}

fn invalid(store: &Store, reason: &'static str) -> StorageError {
    StorageError::new(store.path(), "build Matter topology", reason)
}

impl Model {
    pub(super) fn take_rebuild(&self) -> Option<Rebuild> {
        self.rebuild.borrow_mut().take()
    }

    /// Build the live handlers for one resolved entry. This only reads the
    /// already-committed registry handles; a missing group handle would mean
    /// the commit itself failed to install the prepared plan.
    fn build_entry(
        &self,
        desired: &DesiredEntry,
        applied: &crate::device::AppliedConfiguration,
    ) -> Entry {
        let mut random = rand::rng();
        let versions = Versions {
            descriptor: random.random(),
            groups: random.random(),
            identify: random.random(),
            on_off: random.random(),
            bridged: random.random(),
            level: random.random(),
            color: random.random(),
        };
        let role = match &desired.kind {
            DesiredKind::Device {
                device,
                function,
                stored,
                mapping,
            } => {
                let control = Control::new(
                    function.clone(),
                    desired.endpoint,
                    versions,
                    *mapping,
                    self.executor.clone(),
                );
                let bridged = self.device_bridge(desired, device, stored, versions.bridged);
                Role::Single { control, bridged }
            }
            DesiredKind::Parent {
                device,
                stored,
                children,
            } => {
                let bridged = self.device_bridge(desired, device, stored, versions.bridged);
                Role::Parent {
                    children: children.clone(),
                    bridged,
                }
            }
            DesiredKind::Child {
                function,
                index,
                label,
            } => {
                let control = Control::new(
                    function.clone(),
                    desired.endpoint,
                    versions,
                    None,
                    self.executor.clone(),
                );
                Role::Child {
                    control,
                    index: *index,
                    label: label.clone(),
                }
            }
            DesiredKind::Group {
                key,
                identity,
                mapping,
            } => {
                let group = applied
                    .groups
                    .iter()
                    .find(|group| &group.key == key)
                    .expect("a prepared group is installed by the same commit")
                    .handle()
                    .clone();
                let bridged = BridgedHandler::new(
                    BridgeTarget::Group {
                        group: group.clone(),
                        registry: self.registry.clone(),
                    },
                    Dataver::new(versions.bridged),
                    desired.configuration_version,
                    desired.endpoint,
                    identity.clone(),
                    self.store.devices(),
                    self.protocol.clone(),
                );
                let control = Control::new_group(
                    group,
                    desired.endpoint,
                    versions,
                    *mapping,
                    self.executor.clone(),
                );
                Role::Single { control, bridged }
            }
        };
        Entry {
            endpoint: desired.endpoint,
            descriptor: Dataver::new(versions.descriptor),
            role,
            shape: desired.shape(),
        }
    }

    fn device_bridge(
        &self,
        desired: &DesiredEntry,
        device: &PhysicalDevice,
        stored: &StoredDevice,
        version: u32,
    ) -> BridgedHandler {
        BridgedHandler::new(
            BridgeTarget::Device(device.clone()),
            Dataver::new(version),
            desired.configuration_version,
            stored.endpoint,
            stored.unique_id.clone(),
            self.store.devices(),
            self.protocol.clone(),
        )
    }

    /// Establish the complete persisted shape/version boundary before serving
    /// exchanges or resumed subscriptions. A baseline commit groups the
    /// canonical shape, the root BasicInfo bump and affected endpoint versions
    /// into one transaction; a repeated call with the same shape is a no-op.
    pub(super) fn initialize(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let stored = self
            .protocol
            .record(self.store.matter().get(MATTER_SHAPE_KEY))?;
        let entries = self
            .entries
            .iter()
            .map(|entry| (entry.shape(), entry.configuration_version()))
            .collect::<Vec<_>>();
        let settings = ctx
            .matter()
            .with_state(|state| state.basic_info_settings.clone());
        let changes = self.protocol.record(shape_changes(
            &self.store,
            settings,
            stored.as_deref(),
            &entries,
        ))?;
        let Some(settings) = changes.settings else {
            return Ok(());
        };
        self.protocol.record(
            self.store
                .devices()
                .commit_protocol_configuration(changes.versions.clone(), changes.blobs)
                .map_err(|error| commit_error(&self.store, "commit Matter shape baseline", error)),
        )?;
        ctx.matter()
            .with_state(|state| state.basic_info_settings = settings);
        ctx.notify_attr_changed(0, BASIC_INFO_CLUSTER, BASIC_INFO_CONFIGURATION_VERSION);
        for version in changes.versions {
            let entry = self.entry(version.endpoint)?;
            let bridged = entry
                .role
                .bridged()
                .expect("a light-like entry owns bridged information");
            let changed = bridged.configuration_version_value() != version.version;
            bridged.set_configuration_version(version.version);
            if changed {
                ctx.notify_attr_changed(version.endpoint, BDBI_CLUSTER, BDBI_CONFIGURATION_VERSION);
            }
        }
        Ok(())
    }

    async fn run_devices(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut changes = self.registry.subscribe();
        let mut tasks = JobFutures::new();
        for entry in &self.entries {
            if let Some(control) = entry.role.control() {
                tasks.push(Box::pin(control.run(&ctx)));
            }
            if let Some(bridge) = entry.role.bridged() {
                let ctx = &ctx;
                tasks.push(Box::pin(async move {
                    bridged::HandlerAdaptor(bridge).run(ctx).await
                }));
            }
        }
        let mut retry = self.retry.borrow_mut().take();
        loop {
            let snapshot = self.registry.snapshot();
            if snapshot.pending.is_some() {
                if retry
                    .as_ref()
                    .is_some_and(|state| state.revision != snapshot.revision)
                {
                    retry = None;
                }
                if retry
                    .as_ref()
                    .is_none_or(|state| Instant::now() >= state.deadline)
                {
                    *self.rebuild.borrow_mut() = Some(Rebuild {
                        registry: self.registry.clone(),
                        store: self.store.clone(),
                        retry,
                    });
                    return Ok(());
                }
            } else {
                retry = None;
                self.registry.clear_apply_error();
            }
            let changed = async {
                changes.changed().await;
                Ok::<(), Error>(())
            };
            let retry_due = async {
                match &retry {
                    Some(state) => {
                        async_io::Timer::at(state.deadline).await;
                        Ok(())
                    }
                    None => std::future::pending::<Result<(), Error>>().await,
                }
            };
            let failed = async {
                match tasks.next().await {
                    Some(Err(error)) => Err(error),
                    Some(Ok(())) => Err(ErrorCode::InvalidState.into()),
                    None => std::future::pending().await,
                }
            };
            futures_lite::future::or(changed, futures_lite::future::or(retry_due, failed)).await?;
        }
    }
}
impl Metadata for Model {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let entries = &self.entries;
        let tags = entries
            .iter()
            .map(|entry| match &entry.role {
                Role::Child { index, label, .. } => {
                    vec![SemanticTag::new(NS_COMMON_NUMBER, *index).with_label(label)]
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
        for entry in self.entries.iter().filter(|entry| {
            ctx.endpt()
                .is_none_or(|endpoint| entry.endpoint == endpoint)
        }) {
            entry.bump(ctx.cluster());
        }
    }
    fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
        // Bridged clusters keep no persistent lifecycle state.
        self.root.lifecycle(ctx, op)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.initialize(&ctx)?;
        futures_lite::future::or(self.root.run(&ctx), self.run_devices(&ctx)).await
    }
}
