use super::{bridged_info::BridgedHandler, lighting, on_off::OnOffHandler, sensors};
use crate::device::{Function, FunctionKind, LightProfileKind, PhysicalDevice};
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
                air_quality,
                bridged_device_basic_information::{self as bridged, ClusterHandler as _},
                carbon_dioxide_concentration_measurement, color_control, illuminance_measurement,
                level_control, power_source, relative_humidity_measurement,
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
const CHILD_O_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    lighting::IDENTIFY,
    lighting::OnOffHandler::CLUSTER
);
const CHILD_OB_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    lighting::IDENTIFY,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER
);
const CHILD_OBC_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    lighting::IDENTIFY,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<false> as color_control::ClusterAsyncHandler>::CLUSTER
);
const CHILD_OBCR_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    lighting::IDENTIFY,
    lighting::OnOffHandler::CLUSTER,
    <lighting::LevelHandler as level_control::ClusterAsyncHandler>::CLUSTER,
    <lighting::ColorHandler<true> as color_control::ClusterAsyncHandler>::CLUSTER
);
const PARENT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] =
    clusters!(desc::DescHandler::CLUSTER, BridgedHandler::CLUSTER);
const POWER_SOURCE: DeviceType = DeviceType {
    dtype: 0x0011,
    drev: 1,
};
const PARENT_BATTERY_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    BridgedHandler::CLUSTER,
    <sensors::BatteryHandler as power_source::ClusterHandler>::CLUSTER
);
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

const AIR_QUALITY_SENSOR: DeviceType = DeviceType {
    dtype: 0x002c,
    drev: 1,
};
const CO2_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <sensors::AirQualityHandler as air_quality::ClusterHandler>::CLUSTER,
    <sensors::Co2Handler as carbon_dioxide_concentration_measurement::ClusterHandler>::CLUSTER
);

const LIGHT_SENSOR: DeviceType = DeviceType {
    dtype: 0x0106,
    drev: 3,
};
const ILLUMINANCE_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <sensors::IlluminanceHandler as illuminance_measurement::ClusterHandler>::CLUSTER
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
/// Restore BasicInfoSettings using upstream recovery and align it to the
/// committed root version, preserving other fields of a valid record.
/// A valid record newer than the committed root is rejected. This does not
/// write; the caller installs and persists the settings after model startup.
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

/// Persist the prepared settings, propagating serialization and storage errors.
pub(super) fn persist_basic_info(
    kv: impl KvBlobStoreAccess,
    settings: &BasicInfoSettings,
) -> Result<(), Error> {
    persist::Persist::strict(kv).store_tlv(persist::BASIC_INFO_KEY, settings)
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
    illuminance: u32,
    co2: u32,
    air_quality: u32,
    power_source: u32,
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
            illuminance: random.random(),
            co2: random.random(),
            air_quality: random.random(),
            power_source: random.random(),
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
        function: FunctionHandler,
        bridged: BridgedHandler,
    },
    Parent {
        children: Vec<u16>,
        bridged: BridgedHandler,
        battery: Option<sensors::BatteryHandler>,
    },
    Child {
        function: FunctionHandler,
        tag: u8,
        label: String,
    },
}
struct FunctionHandler {
    groups: Dataver,
    identify: identify::IdentifyHandler,
    kind: FunctionHandlerKind,
}
enum FunctionHandlerKind {
    Outlet(OnOffHandler),
    Light(Box<lighting::Lighting>),
    Temperature(sensors::TemperatureHandler),
    Humidity(sensors::HumidityHandler),
    Illuminance(sensors::IlluminanceHandler),
    Co2(sensors::Co2Sensor),
}
impl FunctionHandler {
    fn new(function: Function, endpoint: u16, versions: Versions) -> Self {
        let kind = match function.description().kind {
            FunctionKind::Light(_) => FunctionHandlerKind::Light(Box::new(
                lighting::Lighting::new(
                    function,
                    endpoint,
                    Dataver::new(versions.on_off),
                    Dataver::new(versions.level),
                    Dataver::new(versions.color),
                )
                .expect("validated published lighting capabilities"),
            )),
            FunctionKind::Temperature(range) => {
                FunctionHandlerKind::Temperature(sensors::TemperatureHandler::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.temperature),
                ))
            }
            FunctionKind::Humidity(range) => {
                FunctionHandlerKind::Humidity(sensors::HumidityHandler::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.humidity),
                ))
            }
            FunctionKind::Illuminance(range) => {
                FunctionHandlerKind::Illuminance(sensors::IlluminanceHandler::new_illuminance(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.illuminance),
                ))
            }
            FunctionKind::CarbonDioxide(range) => {
                FunctionHandlerKind::Co2(sensors::Co2Sensor::new(
                    function,
                    range,
                    endpoint,
                    Dataver::new(versions.co2),
                    Dataver::new(versions.air_quality),
                ))
            }
            FunctionKind::Outlet => FunctionHandlerKind::Outlet(OnOffHandler::new(
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
            FunctionHandlerKind::Light(light) => Some(light),
            FunctionHandlerKind::Outlet(_)
            | FunctionHandlerKind::Temperature(_)
            | FunctionHandlerKind::Humidity(_)
            | FunctionHandlerKind::Co2(_)
            | FunctionHandlerKind::Illuminance(_) => None,
        }
    }
    fn is_sensor(&self) -> bool {
        matches!(
            self.kind,
            FunctionHandlerKind::Temperature(_)
                | FunctionHandlerKind::Humidity(_)
                | FunctionHandlerKind::Co2(_)
                | FunctionHandlerKind::Illuminance(_)
        )
    }
    fn handler(&self) -> impl AsyncHandler + '_ {
        let matcher: fn(u16, u32) -> bool = match &self.kind {
            FunctionHandlerKind::Outlet(_) => |_, cluster| cluster == 6,
            FunctionHandlerKind::Light(_) => |_, cluster| matches!(cluster, 6 | 8 | 0x300),
            FunctionHandlerKind::Temperature(_) => |_, cluster| cluster == 0x402,
            FunctionHandlerKind::Humidity(_) => |_, cluster| cluster == 0x405,
            FunctionHandlerKind::Illuminance(_) => |_, cluster| cluster == 0x400,
            FunctionHandlerKind::Co2(_) => |_, cluster| matches!(cluster, 0x040d | 0x005b),
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
impl AsyncHandler for FunctionHandler {
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
macro_rules! dispatch_function {
    ($kind:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match $kind {
            FunctionHandlerKind::Outlet(handler) => AsyncHandler::$method(
                &on_off::HandlerAsyncAdaptor(handler), $($arg),*
            ).await,
            FunctionHandlerKind::Light(light) => AsyncHandler::$method(
                &lighting::Dispatch(light), $($arg),*
            ).await,
            FunctionHandlerKind::Temperature(handler) => AsyncHandler::$method(
                &Async(temperature_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Humidity(handler) => AsyncHandler::$method(
                &Async(relative_humidity_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Illuminance(handler) => AsyncHandler::$method(
                &Async(illuminance_measurement::HandlerAdaptor(handler)), $($arg),*
            ).await,
            FunctionHandlerKind::Co2(handler) => AsyncHandler::$method(handler, $($arg),*).await,
        }
    };
}
impl AsyncHandler for FunctionHandlerKind {
    fn bump_dataver(&self, _ctx: impl MatchContext) { /* Model owns the retained cluster versions. */
    }
    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        dispatch_function!(self, read(ctx, reply))
    }
    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        dispatch_function!(self, write(ctx))
    }
    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        dispatch_function!(self, invoke(ctx, reply))
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        dispatch_function!(self, run(ctx))
    }
}
impl Role {
    fn function(&self) -> Option<&FunctionHandler> {
        match self {
            Self::Single { function, .. } | Self::Child { function, .. } => Some(function),
            Self::Parent { .. } => None,
        }
    }
    fn bridged(&self) -> Option<&BridgedHandler> {
        match self {
            Self::Single { bridged, .. } | Self::Parent { bridged, .. } => Some(bridged),
            Self::Child { .. } => None,
        }
    }
    fn battery(&self) -> Option<&sensors::BatteryHandler> {
        match self {
            Self::Parent { battery, .. } => battery.as_ref(),
            Self::Single { .. } | Self::Child { .. } => None,
        }
    }
}
// Each role composes only its supported clusters; rs-matter owns operation routing.
macro_rules! dispatch {
    ($entry:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match &$entry.role {
            Role::Single { function, bridged, .. } => AsyncHandler::$method(
                &ChainedHandler::new(
                    |_, cluster| cluster == 0x39,
                    Async(bridged::HandlerAdaptor(bridged)),
                    function,
                ), $($arg),*
            ).await,
            Role::Parent { bridged, battery, .. } => match battery {
                Some(battery) => AsyncHandler::$method(
                    &EmptyHandler
                        .chain(
                            |_, cluster| cluster == 0x39,
                            Async(bridged::HandlerAdaptor(bridged)),
                        )
                        .chain(
                            |_, cluster| cluster == 0x2f,
                            Async(power_source::HandlerAdaptor(battery)),
                        ), $($arg),*
                ).await,
                None => AsyncHandler::$method(
                    &EmptyHandler.chain(
                        |_, cluster| cluster == 0x39,
                        Async(bridged::HandlerAdaptor(bridged)),
                    ), $($arg),*
                ).await,
            },
            Role::Child { function, .. } => function.$method($($arg),*).await,
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
            Role::Single { function, .. } => {
                if let Some(light) = function.light() {
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
            Role::Parent { battery, .. } => {
                if battery.is_some() {
                    Endpoint::new(
                        self.endpoint,
                        devices!(DEV_TYPE_BRIDGED_NODE, POWER_SOURCE),
                        PARENT_BATTERY_CLUSTERS,
                    )
                } else {
                    Endpoint::new(
                        self.endpoint,
                        devices!(DEV_TYPE_BRIDGED_NODE),
                        PARENT_CLUSTERS,
                    )
                }
            }
            Role::Child { function, .. } => match &function.kind {
                FunctionHandlerKind::Temperature(_) => Endpoint::new(
                    self.endpoint,
                    devices!(TEMPERATURE_SENSOR),
                    TEMPERATURE_CLUSTERS,
                )
                .with_tags(tags),
                FunctionHandlerKind::Humidity(_) => {
                    Endpoint::new(self.endpoint, devices!(HUMIDITY_SENSOR), HUMIDITY_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::Illuminance(_) => {
                    Endpoint::new(self.endpoint, devices!(LIGHT_SENSOR), ILLUMINANCE_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::Co2(_) => {
                    Endpoint::new(self.endpoint, devices!(AIR_QUALITY_SENSOR), CO2_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::Light(light) => match light.profile().kind() {
                    LightProfileKind::OnOff => Endpoint::new(
                        self.endpoint,
                        devices!(DEV_TYPE_ON_OFF_LIGHT),
                        CHILD_O_CLUSTERS,
                    ),
                    LightProfileKind::Dimmable => Endpoint::new(
                        self.endpoint,
                        devices!(DEV_TYPE_DIMMABLE_LIGHT),
                        CHILD_OB_CLUSTERS,
                    ),
                    LightProfileKind::ColorTemperature => {
                        Endpoint::new(self.endpoint, devices!(CT_LIGHT), CHILD_OBC_CLUSTERS)
                    }
                    LightProfileKind::ExtendedColor => {
                        Endpoint::new(self.endpoint, devices!(RGB_LIGHT), CHILD_OBCR_CLUSTERS)
                    }
                }
                .with_tags(tags),
                FunctionHandlerKind::Outlet(_) => {
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
        if let Some(function) = self.role.function() {
            if !function.is_sensor() && cluster.is_none_or(|id| id == 4) {
                function.groups.changed();
            }
            if cluster.is_none_or(|id| id == 3) {
                function.identify.dataver_changed();
            }
            match &function.kind {
                FunctionHandlerKind::Outlet(handler)
                    if cluster.is_none_or(|id| id == 6) =>
                {
                    handler.dataver_changed()
                }
                FunctionHandlerKind::Temperature(handler)
                    if cluster.is_none_or(|id| id == 0x402) =>
                {
                    <sensors::TemperatureHandler as
                        temperature_measurement::ClusterHandler>::dataver_changed(handler)
                }
                FunctionHandlerKind::Humidity(handler)
                    if cluster.is_none_or(|id| id == 0x405) =>
                {
                    <sensors::HumidityHandler as
                        relative_humidity_measurement::ClusterHandler>::dataver_changed(handler)
                }
                FunctionHandlerKind::Illuminance(handler) if cluster.is_none_or(|id| id == 0x400) => {
                    <sensors::IlluminanceHandler as illuminance_measurement::ClusterHandler>::dataver_changed(handler)
                }
                FunctionHandlerKind::Co2(handler) => handler.bump(cluster),
                _ => {}
            }
        }
        if let Some(light) = self.role.function().and_then(FunctionHandler::light) {
            light.bump_version(cluster);
        }
        if let Some(handler) = self.role.bridged()
            && cluster.is_none_or(|id| id == 0x39)
        {
            handler.dataver_changed();
        }
        if let Some(handler) = self.role.battery()
            && cluster.is_none_or(|id| id == 0x2f)
        {
            <sensors::BatteryHandler as power_source::ClusterHandler>::dataver_changed(handler);
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
        let multiple = identity.functions.len() > 1
            || device
                .functions()
                .iter()
                .any(|function| function.description().is_measurement());
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
                None => {
                    let children = identity
                        .functions
                        .iter()
                        .map(|function| function.endpoint)
                        .collect::<Vec<_>>();
                    let battery = device.has_battery().then(|| {
                        sensors::BatteryHandler::new(
                            device.clone(),
                            identity.endpoint,
                            children.clone(),
                            Dataver::new(versions.power_source),
                        )
                        .expect("validated published battery capability")
                    });
                    Role::Parent {
                        children,
                        bridged: bridged(),
                        battery,
                    }
                }
                Some((index, function)) => {
                    let function_device = device.function(&function.id).unwrap();
                    let label = function_device.description().label.clone();
                    let function = FunctionHandler::new(function_device, endpoint, versions);
                    if multiple {
                        Role::Child {
                            function,
                            tag: index as u8 + 1,
                            label,
                        }
                    } else {
                        Role::Single {
                            function,
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
