use super::{bridged_info::BridgedHandler, lighting, on_off::OnOffHandler, storage::StoreAdapter};
use crate::{
    device::{Category, DeviceRegistry, Function, LightProfile, PhysicalDevice},
    storage::{Identity, StorageError, Store, StoredDevice},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use rand::RngExt as _;
use rs_matter::{
    clusters, devices,
    dm::{
        Async, AsyncHandler, ChainedHandler, Dataver, DeviceType, EmptyHandler, Endpoint,
        FnMatcher, Handler, HandlerContext, InvokeContext, InvokeReply, LifecycleOp, MatchContext,
        Metadata, Node, ReadContext, ReadReply, SemanticTag, WriteContext,
        clusters::{
            app::on_off::{self, ClusterAsyncHandler as _},
            basic_info::{BasicInfoConfig, BasicInfoSettings},
            decl::{
                bridged_device_basic_information::{self as bridged, ClusterHandler as _},
                color_control, level_control,
            },
            desc::{self, ClusterHandler as _},
            groups::{self, ClusterHandler as _},
            identify::{self, ClusterHandler as _},
        },
        devices::{
            DEV_TYPE_AGGREGATOR, DEV_TYPE_BRIDGED_NODE, DEV_TYPE_DIMMABLE_LIGHT,
            DEV_TYPE_ON_OFF_LIGHT, test::TEST_DEV_DET,
        },
        endpoints,
        networks::SysNetifs,
    },
    error::{Error, ErrorCode},
    persist::{self, KvBlobStoreAccess},
    root_endpoint,
    utils::sync::DynBase,
};
use std::rc::Rc;

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
macro_rules! light_clusters {
    ($($extra:expr),* $(,)?) => {
        clusters!(
            desc::DescHandler::CLUSTER,
            groups::GroupsHandler::CLUSTER,
            LIGHT_IDENTIFY,
            BridgedHandler::CLUSTER,
            lighting::OnOffHandler::CLUSTER
            $(, $extra)*
        )
    };
}
const O_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = light_clusters!();
const OB_CLUSTERS: &[rs_matter::dm::Cluster<'static>] =
    light_clusters!(<lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER);
const OBC_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = light_clusters!(
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER
);
const OBCR_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = light_clusters!(
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
/// Read the upstream BasicInfoSettings and align it to the committed root
/// version, preserving every other field. A blob newer than the committed
/// root or a malformed blob is current-format corruption and is never
/// overwritten. This does not write; the caller installs the returned value
/// in the Matter state and persists it only after protocol validation.
pub(super) fn prepare_basic_info(
    kv: impl KvBlobStoreAccess,
    root_version: u32,
) -> Result<BasicInfoSettings, Error> {
    let mut persist = persist::Persist::new(kv);
    match persist.load_tlv::<BasicInfoSettings>(persist::BASIC_INFO_KEY)? {
        Some(mut settings) => {
            if settings.configuration_version > root_version {
                return Err(ErrorCode::InvalidData.into());
            }
            settings.configuration_version = root_version;
            Ok(settings)
        }
        None => {
            let mut settings = BasicInfoSettings::new();
            settings
                .node_label
                .push_str("MiGate")
                .map_err(|_| ErrorCode::ConstraintError)?;
            settings.configuration_version = root_version;
            Ok(settings)
        }
    }
}

/// Persist the complete prepared settings after every protocol blob loaded.
pub(super) fn persist_basic_info(
    kv: impl KvBlobStoreAccess,
    settings: &BasicInfoSettings,
) -> Result<(), Error> {
    persist::Persist::new(kv).store_tlv(persist::BASIC_INFO_KEY, settings)
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
impl Versions {
    /// Each new instance seeds independent random per-cluster DataVersions so
    /// changed live state is reported even when the fixed configuration (and
    /// its ConfigurationVersion) is unchanged across a restart.
    fn new(mut random: impl rand::Rng) -> Self {
        Self {
            descriptor: random.random(),
            groups: random.random(),
            identify: random.random(),
            on_off: random.random(),
            bridged: random.random(),
            level: random.random(),
            color: random.random(),
        }
    }
}
struct Entry {
    endpoint: u16,
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
                |_, cluster| cluster == 6 || cluster == 8 || cluster == 0x300,
                LightDispatch(light),
            )
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
                8 if light.level() => {
                    Some(&<lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER)
                }
                0x300 if light.ct() && light.rgb() => Some(
                    &<lighting::ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER,
                ),
                0x300 if light.ct() => Some(
                    &<lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER,
                ),
                _ => None,
            };
            if let Some(cluster) = commands {
                if cluster.command(ctx.cmd().cmd_id).is_none() {
                    return Err(ErrorCode::CommandNotFound.into());
                }
            } else if matches!(ctx.cmd().cluster_id, 6 | 8 | 0x300) {
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
struct ControlHandler<'a>(&'a Control);
struct LightDispatch<'a>(&'a lighting::Lighting);
impl AsyncHandler for LightDispatch<'_> {
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(lighting::OnOffHandler(self.0))
                    .read(ctx, reply)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
                    .read(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                    .read(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                    .read(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        match ctx.attr().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(lighting::OnOffHandler(self.0))
                    .write(ctx)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
                    .write(ctx)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                    .write(ctx)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                    .write(ctx)
                    .await
            }
            _ => Err(ErrorCode::UnsupportedAccess.into()),
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        match ctx.cmd().cluster_id {
            6 => {
                on_off::HandlerAsyncAdaptor(lighting::OnOffHandler(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            8 if self.0.level() => {
                level_control::HandlerAsyncAdaptor(lighting::LevelHandler(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() && self.0.rgb() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<true>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            0x300 if self.0.ct() => {
                color_control::HandlerAsyncAdaptor(lighting::ColorHandler::<false>(self.0))
                    .invoke(ctx, reply)
                    .await
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        on_off::HandlerAsyncAdaptor(lighting::OnOffHandler(self.0))
            .run(ctx)
            .await
    }
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
}
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
                    &ControlHandler(control),
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

/// Why a light cannot map to any of the four standard Matter light types.
fn light_publication_problem(
    description: &crate::device::FunctionDescription,
) -> Option<&'static str> {
    if description.category != Category::Light {
        return None;
    }
    description.light_profile().err()
}

impl Entry {
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        match &self.role {
            Role::Single { control, .. } => {
                if let Some(light) = &control.lighting {
                    match light.profile() {
                        LightProfile::ExtendedColor => Endpoint::new(
                            self.endpoint,
                            devices!(RGB_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OBCR_CLUSTERS,
                        ),
                        LightProfile::ColorTemperature => Endpoint::new(
                            self.endpoint,
                            devices!(CT_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OBC_CLUSTERS,
                        ),
                        LightProfile::Dimmable => Endpoint::new(
                            self.endpoint,
                            devices!(DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OB_CLUSTERS,
                        ),
                        LightProfile::OnOff => Endpoint::new(
                            self.endpoint,
                            devices!(DEV_TYPE_ON_OFF_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            O_CLUSTERS,
                        ),
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
                if let Some(light) = &control.lighting {
                    light.on_off_version.changed();
                } else {
                    control.on_off.as_ref().unwrap().dataver_changed();
                }
            }
        }
        if let Some(light) = self.role.control().and_then(|c| c.lighting.as_ref()) {
            if light.level() && cluster.is_none_or(|id| id == 8) {
                light.level_version.changed();
            }
            if light.ct() && cluster.is_none_or(|id| id == 0x300) {
                light.color_version.changed();
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

/// One published physical device frozen with its committed identity.
#[derive(Clone)]
pub(super) struct FrozenEntry {
    pub(super) device: PhysicalDevice,
    pub(super) stored: StoredDevice,
}

/// Capture the committed published set once; the model never rereads topology
/// or fixed metadata rows afterwards.
pub(super) fn capture(
    registry: &DeviceRegistry,
    store: &Store,
) -> Result<(Vec<FrozenEntry>, u32), StorageError> {
    let mut entries = Vec::new();
    for device in registry.list() {
        let stored = store
            .devices()
            .load_published(device.key())?
            .ok_or_else(|| {
                StorageError::new(
                    store.path(),
                    "freeze Matter topology",
                    "Published physical device has no stored identity",
                )
            })?;
        entries.push(FrozenEntry { device, stored });
    }
    entries.sort_by_key(|entry| entry.stored.endpoint);
    let root_version = store.devices().load_publication(None)?.root_version;
    Ok((entries, root_version))
}

pub(super) struct Model {
    root: RootHandler,
    entries: Vec<Rc<Entry>>,
}

/// Build one frozen instance from the committed published set.
#[cfg(test)]
pub(super) fn handler(
    registry: &DeviceRegistry,
    store: &Store,
    protocol: StoreAdapter,
    random: impl rand::Rng,
) -> Result<Model, StorageError> {
    let (entries, _root_version) = capture(registry, store)?;
    handler_from(entries, store, protocol, random)
}

pub(super) fn handler_from(
    entries: Vec<FrozenEntry>,
    store: &Store,
    protocol: StoreAdapter,
    mut random: impl rand::Rng,
) -> Result<Model, StorageError> {
    let root = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(&mut random)
        .chain(
            |endpoint, cluster| endpoint == AGGREGATOR_ENDPOINT && cluster == 0x1d,
            Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
        );
    let mut built = Vec::new();
    for FrozenEntry { device, stored } in entries {
        if let Some(reason) = device
            .functions()
            .iter()
            .find_map(|function| light_publication_problem(function.description()))
        {
            log::warn!("Not publishing Xiaomi device {}: {reason}", device.key().id);
            continue;
        }
        let versions = Versions::new(&mut random);
        let multiple = stored.functions.len() > 1;
        let parent = multiple.then_some((stored.endpoint, None));
        let functions = stored
            .functions
            .iter()
            .enumerate()
            .map(|(index, function)| (function.endpoint, Some((index, function))));
        for (endpoint, function) in parent.into_iter().chain(functions) {
            let bridged = || {
                BridgedHandler::new(
                    device.clone(),
                    Dataver::new(versions.bridged),
                    &stored,
                    store.devices(),
                    protocol.clone(),
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
            built.push(Rc::new(Entry {
                endpoint,
                descriptor: Dataver::new(versions.descriptor),
                role,
            }));
        }
    }
    built.sort_by_key(|entry| entry.endpoint);
    Ok(Model {
        root,
        entries: built,
    })
}
impl Model {
    fn entry(&self, endpoint: u16) -> Result<Rc<Entry>, Error> {
        self.entries
            .iter()
            .find(|entry| entry.endpoint == endpoint)
            .cloned()
            .ok_or_else(|| ErrorCode::EndpointNotFound.into())
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
        // Frozen clusters keep no persistent lifecycle state.
        self.root.lifecycle(ctx, op)
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut tasks = FuturesUnordered::new();
        for entry in &self.entries {
            let entry = entry.clone();
            let context = &ctx;
            tasks.push(async move { entry.run(context).await });
        }
        futures_lite::future::or(self.root.run(&ctx), async {
            if let Some(result) = tasks.next().await {
                result?;
                return Err(ErrorCode::InvalidState.into());
            }
            std::future::pending().await
        })
        .await
    }
}
