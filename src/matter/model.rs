use super::{bridged_info::BridgedHandler, lighting, on_off::OnOffHandler, sensors};
use crate::device::{Function, LightProfileKind, PhysicalDevice};
use crate::storage::Identity;
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
                color_control, level_control, relative_humidity_measurement,
                temperature_measurement,
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
macro_rules! light_clusters {
    ($($extra:expr),* $(,)?) => {
        clusters!(
            desc::DescHandler::CLUSTER,
            groups::GroupsHandler::CLUSTER,
            lighting::IDENTIFY,
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
const TEMPERATURE_SENSOR: DeviceType = DeviceType {
    dtype: 0x0302,
    drev: 3,
};
const HUMIDITY_SENSOR: DeviceType = DeviceType {
    dtype: 0x0307,
    drev: 2,
};
const TEMPERATURE_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <sensors::TemperatureHandler as temperature_measurement::ClusterHandler>::CLUSTER
);
const HUMIDITY_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <sensors::HumidityHandler as relative_humidity_measurement::ClusterHandler>::CLUSTER
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
    temperature: u32,
    humidity: u32,
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
            temperature: random.random(),
            humidity: random.random(),
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
    kind: ControlKind,
}
enum ControlKind {
    Outlet(OnOffHandler),
    Light(Box<lighting::Lighting>),
    Temperature(sensors::TemperatureHandler),
    Humidity(sensors::HumidityHandler),
}
impl Control {
    fn new(function: Function, endpoint: u16, versions: Versions) -> Self {
        let kind = match function.description().kind {
            crate::device::FunctionKind::Light(_) => ControlKind::Light(Box::new(
                lighting::Lighting::new(
                    function,
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                )
                .expect("validated published lighting capabilities"),
            )),
            crate::device::FunctionKind::Temperature(_) => ControlKind::Temperature(
                sensors::TemperatureHandler::new(
                    function,
                    endpoint,
                    Dataver::new(versions.temperature),
                )
                .expect("validated published temperature range"),
            ),
            crate::device::FunctionKind::Humidity(_) => ControlKind::Humidity(
                sensors::HumidityHandler::new(function, endpoint, Dataver::new(versions.humidity))
                    .expect("validated published humidity range"),
            ),
            crate::device::FunctionKind::Outlet => ControlKind::Outlet(OnOffHandler::new(
                function,
                endpoint,
                Dataver::new(versions.on_off),
            )),
        };
        Self {
            groups: Dataver::new(versions.groups),
            identify: identify::IdentifyHandler::new(Dataver::new(versions.identify)),
            kind,
        }
    }
    fn light(&self) -> Option<&lighting::Lighting> {
        match &self.kind {
            ControlKind::Light(light) => Some(light),
            ControlKind::Outlet(_) | ControlKind::Temperature(_) | ControlKind::Humidity(_) => None,
        }
    }
    fn is_sensor(&self) -> bool {
        matches!(
            self.kind,
            ControlKind::Temperature(_) | ControlKind::Humidity(_)
        )
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        let matcher: fn(u16, u32) -> bool = match &self.kind {
            ControlKind::Outlet(_) => |_, cluster| cluster == 6,
            ControlKind::Light(_) => |_, cluster| matches!(cluster, 6 | 8 | 0x300),
            ControlKind::Temperature(_) => |_, cluster| cluster == 0x402,
            ControlKind::Humidity(_) => |_, cluster| cluster == 0x405,
        };
        let groups_matcher: fn(u16, u32) -> bool = if self.is_sensor() {
            |_, _| false
        } else {
            |_, cluster| cluster == 4
        };
        EmptyHandler
            .chain(
                groups_matcher,
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
            .chain(matcher, &self.kind)
    }
}
impl AsyncHandler for Control {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        self.handler().read(ctx, reply).await
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        self.handler().write(ctx).await
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        if self.light().is_some()
            && ctx.cmd().cluster_id == 3
            && lighting::IDENTIFY.command(ctx.cmd().cmd_id).is_none()
        {
            return Err(ErrorCode::CommandNotFound.into());
        }
        self.handler().invoke(ctx, reply).await
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.handler().run(ctx).await
    }
}
impl AsyncHandler for ControlKind {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        match self {
            Self::Outlet(handler) => on_off::HandlerAsyncAdaptor(handler).read(ctx, reply).await,
            Self::Light(light) => lighting::Dispatch(light).read(ctx, reply).await,
            Self::Temperature(handler) => {
                AsyncHandler::read(
                    &Async(temperature_measurement::HandlerAdaptor(handler)),
                    ctx,
                    reply,
                )
                .await
            }
            Self::Humidity(handler) => {
                AsyncHandler::read(
                    &Async(relative_humidity_measurement::HandlerAdaptor(handler)),
                    ctx,
                    reply,
                )
                .await
            }
        }
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        match self {
            Self::Outlet(handler) => on_off::HandlerAsyncAdaptor(handler).write(ctx).await,
            Self::Light(light) => lighting::Dispatch(light).write(ctx).await,
            Self::Temperature(handler) => {
                AsyncHandler::write(
                    &Async(temperature_measurement::HandlerAdaptor(handler)),
                    ctx,
                )
                .await
            }
            Self::Humidity(handler) => {
                AsyncHandler::write(
                    &Async(relative_humidity_measurement::HandlerAdaptor(handler)),
                    ctx,
                )
                .await
            }
        }
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        match self {
            Self::Outlet(handler) => {
                on_off::HandlerAsyncAdaptor(handler)
                    .invoke(ctx, reply)
                    .await
            }
            Self::Light(light) => lighting::Dispatch(light).invoke(ctx, reply).await,
            Self::Temperature(handler) => {
                AsyncHandler::invoke(
                    &Async(temperature_measurement::HandlerAdaptor(handler)),
                    ctx,
                    reply,
                )
                .await
            }
            Self::Humidity(handler) => {
                AsyncHandler::invoke(
                    &Async(relative_humidity_measurement::HandlerAdaptor(handler)),
                    ctx,
                    reply,
                )
                .await
            }
        }
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        match self {
            Self::Outlet(handler) => on_off::HandlerAsyncAdaptor(handler).run(ctx).await,
            Self::Light(light) => lighting::Dispatch(light).run(ctx).await,
            Self::Temperature(handler) => {
                AsyncHandler::run(
                    &Async(temperature_measurement::HandlerAdaptor(handler)),
                    ctx,
                )
                .await
            }
            Self::Humidity(handler) => {
                AsyncHandler::run(
                    &Async(relative_humidity_measurement::HandlerAdaptor(handler)),
                    ctx,
                )
                .await
            }
        }
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

/// Why a light cannot map to any of the four standard Matter light types.
impl Entry {
    fn descriptor(&self) -> desc::DescHandler<'_> {
        desc::DescHandler::new_matching(Dataver::new(self.descriptor.get()), self)
    }
    fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        match &self.role {
            Role::Single { control, .. } => {
                if let Some(light) = control.light() {
                    match light.profile().kind() {
                        LightProfileKind::ExtendedColor => Endpoint::new(
                            self.endpoint,
                            devices!(RGB_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OBCR_CLUSTERS,
                        ),
                        LightProfileKind::ColorTemperature => Endpoint::new(
                            self.endpoint,
                            devices!(CT_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OBC_CLUSTERS,
                        ),
                        LightProfileKind::Dimmable => Endpoint::new(
                            self.endpoint,
                            devices!(DEV_TYPE_DIMMABLE_LIGHT, DEV_TYPE_BRIDGED_NODE),
                            OB_CLUSTERS,
                        ),
                        LightProfileKind::OnOff => Endpoint::new(
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
            Role::Child { control, .. } => match &control.kind {
                ControlKind::Temperature(_) => Endpoint::new(
                    self.endpoint,
                    devices!(TEMPERATURE_SENSOR),
                    TEMPERATURE_CLUSTERS,
                )
                .with_tags(tags),
                ControlKind::Humidity(_) => {
                    Endpoint::new(self.endpoint, devices!(HUMIDITY_SENSOR), HUMIDITY_CLUSTERS)
                        .with_tags(tags)
                }
                ControlKind::Outlet(_) | ControlKind::Light(_) => {
                    Endpoint::new(self.endpoint, devices!(ON_OFF_PLUG), CHILD_CLUSTERS)
                        .with_tags(tags)
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
            if !control.is_sensor() && cluster.is_none_or(|id| id == 4) {
                control.groups.changed();
            }
            if cluster.is_none_or(|id| id == 3) {
                control.identify.dataver_changed();
            }
            match &control.kind {
                ControlKind::Outlet(handler)
                    if cluster.is_none_or(|id| id == 6) =>
                {
                    handler.dataver_changed()
                }
                ControlKind::Temperature(handler)
                    if cluster.is_none_or(|id| id == 0x402) =>
                {
                    <sensors::TemperatureHandler as
                        temperature_measurement::ClusterHandler>::dataver_changed(handler)
                }
                ControlKind::Humidity(handler)
                    if cluster.is_none_or(|id| id == 0x405) =>
                {
                    <sensors::HumidityHandler as
                        relative_humidity_measurement::ClusterHandler>::dataver_changed(handler)
                }
                _ => {}
            }
        }
        if let Some(light) = self.role.control().and_then(Control::light) {
            light.bump_version(cluster);
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

/// One current published function as Matter publishes it: the stable key and
/// its reserved endpoint. Retired reservations stay in storage only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedFunction {
    pub(crate) id: String,
    pub(crate) endpoint: u16,
}

/// The explicit Matter publication input of one device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedIdentity {
    pub(crate) endpoint: u16,
    pub(crate) unique_id: String,
    pub(crate) configuration_version: u32,
    pub(crate) functions: Vec<PublishedFunction>,
}

/// One published physical device frozen with its current publication identity.
#[derive(Clone)]
pub(crate) struct FrozenEntry {
    pub(crate) device: PhysicalDevice,
    pub(crate) identity: PublishedIdentity,
}

pub(super) struct Model {
    root: RootHandler,
    entries: Vec<Rc<Entry>>,
}

pub(super) fn handler_from(
    entries: Vec<FrozenEntry>,
    mut random: impl rand::Rng,
    renamer: Rc<dyn super::bridge::LabelWriter>,
) -> Model {
    let root = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(&mut random)
        .chain(
            |endpoint, cluster| endpoint == AGGREGATOR_ENDPOINT && cluster == 0x1d,
            Async(desc::DescHandler::new_aggregator(Dataver::new_rand(&mut random)).adapt()),
        );
    let mut built = Vec::new();
    for FrozenEntry { device, identity } in entries {
        let versions = Versions::new(&mut random);
        let multiple = identity.functions.len() > 1;
        let parent = multiple.then_some((identity.endpoint, None));
        let functions = identity
            .functions
            .iter()
            .enumerate()
            .map(|(index, function)| (function.endpoint, Some((index, function))));
        for (endpoint, function) in parent.into_iter().chain(functions) {
            let bridged = || {
                BridgedHandler::new(
                    device.clone(),
                    Dataver::new(versions.bridged),
                    &identity,
                    renamer.clone(),
                )
            };
            let role = match function {
                None => Role::Parent {
                    children: identity
                        .functions
                        .iter()
                        .map(|function| function.endpoint)
                        .collect(),
                    bridged: bridged(),
                },
                Some((index, function)) => {
                    let function_device = device.function(&function.id).unwrap();
                    let label = function_device.description().label.clone();
                    let control = Control::new(function_device, endpoint, versions);
                    if multiple {
                        Role::Child {
                            control,
                            tag: index as u8 + 1,
                            label,
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
    Model {
        root,
        entries: built,
    }
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
