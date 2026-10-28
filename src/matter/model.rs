use super::{
    bridged_info::BridgedHandler, curtain, lighting, on_off::OnOffHandler, sensors, states,
};
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
                air_quality, boolean_state,
                bridged_device_basic_information::{self as bridged, ClusterHandler as _},
                carbon_dioxide_concentration_measurement, color_control, illuminance_measurement,
                level_control, occupancy_sensing, power_source, relative_humidity_measurement,
                temperature_measurement, window_covering,
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

mod basic_info;
mod endpoint;
mod function;

pub(super) use basic_info::{basic_info, persist_basic_info, prepare_basic_info};
use endpoint::{Entry, Role};
use function::{FunctionHandler, FunctionHandlerKind};

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
    state: u32,
    curtain: u32,
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
            state: random.random(),
            curtain: random.random(),
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
                .any(|function| function.description().is_read_only());
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
