use super::{
    common, lighting, model, sensors,
    storage::{EndpointSceneStore, StoreAdapter, TopologyStore},
};
use crate::{
    RuntimeError,
    device::{Capability, DeviceChange, DeviceService, FeatureIdentity, FeatureRole, Property},
    storage::{
        DeviceStore, FeatureIdentity as AllocatedFeature, Identity, MatterStore, StorageError,
        Store,
    },
};
use event_listener::Event;
use futures_lite::future;
use rand::RngExt as _;
use rs_matter::dm::clusters::{desc::ClusterHandler as _, identify::ClusterHandler as _};
use rs_matter::{
    Matter,
    crypto::{Crypto, default_crypto},
    dm::{
        AsyncHandler, AttrChangeNotifier, AttrDetails, Cluster, CmdDetails, Dataver, DeviceType,
        Endpoint, EventEmitter, EventNumber, Handler, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, Metadata, Node, OperationContext, OwnAttrChangeNotifier, OwnEventEmitter,
        ReadContext, ReadReply, Reply, WriteContext,
        clusters::decl::{
            boolean_state, bridged_device_basic_information as bridged, illuminance_measurement,
            occupancy_sensing, power_source, relative_humidity_measurement,
            temperature_measurement,
        },
        clusters::{desc, groups, identify, scenes},
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        devices::{DEV_TYPE_BRIDGED_NODE, DEV_TYPE_POWER_SOURCE},
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    error::{Error, ErrorCode},
    im::{
        EthInteractionModelState, ImStats, InteractionModel,
        encoding::{EventPriority, IMBuffer},
        events::EventTLVWrite,
    },
    persist::{BASIC_INFO_KEY, KvBlobStoreAccess},
    respond::DefaultResponder,
    tlv::{TLVControl, TLVElement, TLVTagType, TLVValueType, TLVWrite, TagType, ToTLV},
    transport::{
        MATTER_SOCKET_BIND_ADDR,
        exchange::{Exchange, MatterBuffers},
        network::mdns::astro::AstroMdns,
    },
    utils::storage::pooled::Buffers,
};
use sha1::{Digest, Sha1};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    net::UdpSocket,
    num::NonZeroU8,
    rc::Rc,
    time::Duration,
};

const WINDOW_SECONDS: u16 = 900;

const DEV_TYPE_TEMPERATURE_SENSOR: DeviceType = DeviceType {
    dtype: 0x0302,
    drev: 2,
};
const DEV_TYPE_HUMIDITY_SENSOR: DeviceType = DeviceType {
    dtype: 0x0307,
    drev: 2,
};
const DEV_TYPE_LIGHT_SENSOR: DeviceType = DeviceType {
    dtype: 0x0106,
    drev: 3,
};
const DEV_TYPE_OCCUPANCY_SENSOR: DeviceType = DeviceType {
    dtype: 0x0107,
    drev: 4,
};
const DEV_TYPE_CONTACT_SENSOR: DeviceType = DeviceType {
    dtype: 0x0015,
    drev: 2,
};
const DEV_TYPE_ON_OFF_LIGHT: DeviceType = DeviceType {
    dtype: 0x0100,
    drev: 3,
};
const DEV_TYPE_DIMMABLE_LIGHT: DeviceType = DeviceType {
    dtype: 0x0101,
    drev: 3,
};
const DEV_TYPE_COLOR_TEMPERATURE_LIGHT: DeviceType = DeviceType {
    dtype: 0x010c,
    drev: 3,
};
const DEV_TYPE_EXTENDED_COLOR_LIGHT: DeviceType = DeviceType {
    dtype: 0x010d,
    drev: 4,
};
const DEV_TYPE_ON_OFF_PLUGIN_UNIT: DeviceType = DeviceType {
    dtype: 0x010a,
    drev: 3,
};

struct SceneContext<'a, C> {
    base: &'a C,
    store: EndpointSceneStore,
}

impl<'a, C> SceneContext<'a, C> {
    fn new(base: &'a C, store: StoreAdapter, endpoint: u16) -> Self {
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

struct SceneValidReadReply<R>(R);

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

struct FeatureRuntime {
    allocation: AllocatedFeature,
    device_types: Vec<DeviceType>,
    clusters: Vec<Cluster<'static>>,
    shape_signature: String,
    config_signature: RefCell<String>,
    exposed_values: RefCell<BTreeMap<Property, Option<crate::device::PropertyValue>>>,
    reachable: Cell<bool>,
    desc: desc::DescHandler<'static>,
    identify: identify::IdentifyHandler,
    common: common::CommonHandler,
    sensor: sensors::SensorHandler,
    lighting: Option<lighting::LightingHandler>,
    groups: groups::GroupsHandler<'static>,
    scenes: scenes::ScenesState<33, 96>,
    scenes_dataver: Dataver,
}

impl FeatureRuntime {
    fn lighting(&self) -> &lighting::LightingHandler {
        self.lighting
            .as_ref()
            .expect("lighting clusters require a lighting handler")
    }
}

/// Dynamic Matter metadata and handlers backed by the authoritative device service.
pub struct DeviceBridgeModel {
    service: DeviceService,
    store: StoreAdapter,
    devices: DeviceStore,
    runtimes: RefCell<Rc<Vec<Rc<FeatureRuntime>>>>,
    aggregator_desc: desc::DescHandler<'static>,
    rebuild_requested: Cell<bool>,
    rebuild_event: Event,
    topology_dirty: Cell<bool>,
    requested_topology_signature: RefCell<Option<String>>,
}

impl DeviceBridgeModel {
    async fn run_feature_backgrounds(
        runtimes: Rc<Vec<Rc<FeatureRuntime>>>,
        ctx: &impl HandlerContext,
    ) -> Result<(), Error> {
        let mut futures = Vec::with_capacity(runtimes.len() * 2);
        for runtime in runtimes.iter() {
            futures.push(
                Box::pin(identify::ClusterHandler::run(&runtime.identify, ctx))
                    as std::pin::Pin<Box<dyn Future<Output = Result<(), Error>> + '_>>,
            );
            if let Some(lighting) = &runtime.lighting {
                futures.push(Box::pin(lighting.run(ctx)));
            }
        }
        if futures.is_empty() {
            std::future::pending().await
        } else {
            let (result, _, _) = futures_util::future::select_all(futures).await;
            result
        }
    }

    pub fn new(
        service: DeviceService,
        devices: DeviceStore,
        store: MatterStore,
    ) -> Result<Self, StorageError> {
        Self::with_store(service, devices, StoreAdapter::new(store))
    }

    fn with_store(
        service: DeviceService,
        devices: DeviceStore,
        adapter: StoreAdapter,
    ) -> Result<Self, StorageError> {
        let loaded = adapter.capture(devices.load_features(true))?;
        let allocations = loaded
            .into_iter()
            .filter(|allocation| allocation.active)
            .map(|allocation| (allocation.feature.clone(), allocation))
            .collect::<BTreeMap<_, _>>();
        let model = Self {
            service,
            store: adapter,
            devices,
            runtimes: RefCell::new(Rc::new(Vec::new())),
            aggregator_desc: desc::DescHandler::new_aggregator(Dataver::new(rand::rng().random())),
            rebuild_requested: Cell::new(false),
            rebuild_event: Event::new(),
            topology_dirty: Cell::new(false),
            requested_topology_signature: RefCell::new(None),
        };
        model.rebuild_initial(&allocations)?;
        let topology = model
            .store
            .capture(model.store.storage().topology_signature())?;
        model
            .topology_dirty
            .set(topology != model.topology_signature());
        Ok(model)
    }

    fn rebuild_initial(
        &self,
        allocations: &BTreeMap<FeatureIdentity, AllocatedFeature>,
    ) -> Result<(), StorageError> {
        let mut runtimes = Vec::new();
        for (feature, allocation) in allocations {
            if let Some(published) = self.service.feature(feature)
                && let Some(runtime) = self.build_runtime(
                    allocation.clone(),
                    published.name,
                    published.capabilities.0,
                )?
            {
                runtimes.push(Rc::new(runtime));
            }
        }
        runtimes.sort_by_key(|runtime| runtime.allocation.endpoint);
        *self.runtimes.borrow_mut() = Rc::new(runtimes);
        Ok(())
    }

    fn build_runtime(
        &self,
        allocation: AllocatedFeature,
        name: String,
        capabilities: Vec<Capability>,
    ) -> Result<Option<FeatureRuntime>, StorageError> {
        if !matches!(
            allocation.feature.role,
            FeatureRole::Light
                | FeatureRole::BathHeaterLight
                | FeatureRole::Load
                | FeatureRole::TemperatureSensor
                | FeatureRole::HumiditySensor
                | FeatureRole::IlluminanceSensor
                | FeatureRole::MotionSensor
                | FeatureRole::OccupancySensor
                | FeatureRole::ContactSensor
        ) {
            return Ok(None);
        }
        let mut device_types = Vec::new();
        let mut clusters = vec![desc::CLUSTER_ENDPOINT_UNIQUE_ID, common::BRIDGED_CLUSTER];
        let has_temperature = allocation.feature.role == FeatureRole::TemperatureSensor
            && capabilities
                .iter()
                .any(|cap| matches!(cap, Capability::Temperature(_)));
        let has_humidity = allocation.feature.role == FeatureRole::HumiditySensor
            && capabilities
                .iter()
                .any(|cap| matches!(cap, Capability::Humidity(_)));
        let has_lux = allocation.feature.role == FeatureRole::IlluminanceSensor
            && capabilities
                .iter()
                .any(|cap| matches!(cap, Capability::Illuminance(_)));
        let has_occupancy = matches!(
            allocation.feature.role,
            FeatureRole::MotionSensor | FeatureRole::OccupancySensor
        ) && capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Motion | Capability::Occupancy));
        let has_contact = allocation.feature.role == FeatureRole::ContactSensor
            && capabilities.contains(&Capability::Contact);
        let has_battery = capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Battery(_)));
        let has_power = matches!(
            allocation.feature.role,
            FeatureRole::Light | FeatureRole::BathHeaterLight | FeatureRole::Load
        ) && capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Power { writable: true }));
        let has_level = has_power
            && capabilities
                .iter()
                .any(|cap| matches!(cap, Capability::Brightness(_)));
        let has_temperature_color = has_power
            && capabilities
                .iter()
                .any(|cap| matches!(cap, Capability::ColorTemperature(_)));
        let has_xy = has_power && capabilities.contains(&Capability::Color);
        if !(has_temperature
            || has_humidity
            || has_lux
            || has_occupancy
            || has_contact
            || has_power)
        {
            return Ok(None);
        }
        clusters.push(identify::IdentifyHandler::<()>::CLUSTER);
        if has_power {
            clusters.extend([
                lighting::GROUPS_CLUSTER,
                lighting::SCENES_CLUSTER,
                lighting::ON_OFF_CLUSTER,
            ]);
            if has_level {
                clusters.push(lighting::LEVEL_CLUSTER);
            }
            if has_temperature_color || has_xy {
                clusters.push(lighting::color_cluster(has_xy, has_temperature_color));
            }
            if allocation.feature.role == FeatureRole::Load {
                device_types.push(DEV_TYPE_ON_OFF_PLUGIN_UNIT);
            } else if has_xy && has_temperature_color && has_level {
                device_types.push(DEV_TYPE_EXTENDED_COLOR_LIGHT);
            } else if has_temperature_color && has_level {
                device_types.push(DEV_TYPE_COLOR_TEMPERATURE_LIGHT);
            } else if has_level {
                device_types.push(DEV_TYPE_DIMMABLE_LIGHT);
            } else {
                device_types.push(DEV_TYPE_ON_OFF_LIGHT);
            }
        }
        if has_temperature {
            device_types.push(DEV_TYPE_TEMPERATURE_SENSOR);
            clusters.push(sensors::TEMPERATURE_CLUSTER);
        }
        if has_humidity {
            device_types.push(DEV_TYPE_HUMIDITY_SENSOR);
            clusters.push(sensors::HUMIDITY_CLUSTER);
        }
        if has_lux {
            device_types.push(DEV_TYPE_LIGHT_SENSOR);
            clusters.push(sensors::ILLUMINANCE_CLUSTER);
        }
        if has_occupancy {
            device_types.push(DEV_TYPE_OCCUPANCY_SENSOR);
            let modalities = capabilities
                .iter()
                .find_map(|cap| match cap {
                    Capability::SensingModalities(values) => Some(values.as_slice()),
                    _ => None,
                })
                .unwrap_or_default();
            clusters.push(sensors::occupancy_cluster(modalities));
        }
        if has_contact {
            device_types.push(DEV_TYPE_CONTACT_SENSOR);
            clusters.push(sensors::BOOLEAN_STATE_CLUSTER);
        }
        if has_battery {
            device_types.push(DEV_TYPE_POWER_SOURCE);
            clusters.push(sensors::POWER_SOURCE_CLUSTER);
        }
        device_types.push(DEV_TYPE_BRIDGED_NODE);
        let shape_signature = shape_signature(&device_types, &clusters);
        let config_signature = config_signature(&shape_signature, &capabilities);
        let label_override = self
            .store
            .capture(self.store.storage().feature_label(allocation.endpoint))?;
        let seed = rand::rng().random::<u32>();
        let reachable = self.service.is_available(&allocation.feature);
        let exposed_values = exposed_properties(&capabilities, &clusters)
            .into_iter()
            .map(|property| {
                let value = self
                    .service
                    .snapshot(&allocation.feature)
                    .and_then(|snapshot| snapshot.property(property).cloned())
                    .and_then(|state| match state {
                        crate::device::PropertyState::Current { value, .. } => Some(value),
                        _ => None,
                    });
                (property, value)
            })
            .collect();
        Ok(Some(FeatureRuntime {
            desc: desc::DescHandler::new(Dataver::new(seed)),
            identify: identify::IdentifyHandler::new(Dataver::new(seed.wrapping_add(1))),
            common: common::CommonHandler::new(
                Dataver::new(seed.wrapping_add(2)),
                self.service.clone(),
                allocation.feature.clone(),
                allocation.endpoint,
                allocation.public_id.as_str().to_owned(),
                name,
                label_override,
                self.store.clone(),
            ),
            sensor: sensors::SensorHandler::new(
                self.service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                allocation.endpoint,
                seed.wrapping_add(3),
            ),
            lighting: has_power.then(|| {
                lighting::LightingHandler::new(
                    self.service.clone(),
                    allocation.feature.clone(),
                    capabilities.clone(),
                    allocation.endpoint,
                    seed.wrapping_add(9),
                )
            }),
            groups: groups::GroupsHandler::new(Dataver::new(seed.wrapping_add(12))),
            scenes: scenes::ScenesState::new(),
            scenes_dataver: Dataver::new(seed.wrapping_add(13)),
            allocation,
            device_types,
            clusters,
            shape_signature,
            config_signature: RefCell::new(config_signature),
            exposed_values: RefCell::new(exposed_values),
            reachable: Cell::new(reachable),
        }))
    }

    pub fn endpoint_for(&self, feature: &FeatureIdentity) -> Option<u16> {
        self.snapshot()
            .iter()
            .find(|runtime| &runtime.allocation.feature == feature)
            .map(|runtime| runtime.allocation.endpoint)
    }

    fn snapshot(&self) -> Rc<Vec<Rc<FeatureRuntime>>> {
        self.runtimes.borrow().clone()
    }

    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    pub fn take_rebuild_request(&self) -> bool {
        self.rebuild_requested.replace(false)
    }

    pub async fn rebuild_requested(&self) {
        loop {
            let listener = self.rebuild_event.listen();
            if self.rebuild_requested.get() {
                return;
            }
            listener.await;
        }
    }

    pub fn topology_signature(&self) -> String {
        let runtimes = self.snapshot();
        topology_signature_for(&runtimes)
    }

    async fn dispatch_read(
        &self,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> Result<(), Error> {
        let endpoint = ctx.endpt().ok_or(ErrorCode::AttributeNotFound)?;
        let cluster = ctx.cluster().ok_or(ErrorCode::AttributeNotFound)?;
        if endpoint == 1 && cluster == desc::DescHandler::CLUSTER.id {
            return Handler::read(&self.aggregator_desc.clone().adapt(), ctx, reply);
        }
        let runtimes = self.snapshot();
        let runtime = runtimes
            .iter()
            .find(|runtime| runtime.allocation.endpoint == endpoint)
            .ok_or(ErrorCode::AttributeNotFound)?;
        if runtime
            .clusters
            .iter()
            .find(|candidate| candidate.id == cluster)
            .and_then(|candidate| candidate.attribute(ctx.attr().attr_id))
            .is_none()
        {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        match cluster {
            id if id == desc::DescHandler::CLUSTER.id => {
                Handler::read(&runtime.desc.clone().adapt(), ctx, reply)
            }
            id if id == identify::IdentifyHandler::<()>::CLUSTER.id => {
                Handler::read(&identify::HandlerAdaptor(&runtime.identify), ctx, reply)
            }
            id if id == common::BRIDGED_CLUSTER.id => {
                Handler::read(&bridged::HandlerAdaptor(&runtime.common), ctx, reply)
            }
            id if id == sensors::TEMPERATURE_CLUSTER.id => Handler::read(
                &temperature_measurement::HandlerAdaptor(&runtime.sensor),
                ctx,
                reply,
            ),
            id if id == sensors::HUMIDITY_CLUSTER.id => Handler::read(
                &relative_humidity_measurement::HandlerAdaptor(&runtime.sensor),
                ctx,
                reply,
            ),
            id if id == sensors::ILLUMINANCE_CLUSTER.id => Handler::read(
                &illuminance_measurement::HandlerAdaptor(&runtime.sensor),
                ctx,
                reply,
            ),
            id if id == occupancy_sensing::FULL_CLUSTER.id => Handler::read(
                &occupancy_sensing::HandlerAdaptor(&runtime.sensor),
                ctx,
                reply,
            ),
            id if id == sensors::BOOLEAN_STATE_CLUSTER.id => {
                Handler::read(&boolean_state::HandlerAdaptor(&runtime.sensor), ctx, reply)
            }
            id if id == sensors::POWER_SOURCE_CLUSTER.id => {
                Handler::read(&power_source::HandlerAdaptor(&runtime.sensor), ctx, reply)
            }
            id if id == lighting::ON_OFF_CLUSTER.id => Handler::read(
                &rs_matter::dm::clusters::decl::on_off::HandlerAdaptor(runtime.lighting()),
                ctx,
                reply,
            ),
            id if id == lighting::LEVEL_CLUSTER.id => Handler::read(
                &rs_matter::dm::clusters::decl::level_control::HandlerAdaptor(runtime.lighting()),
                ctx,
                reply,
            ),
            id if id == lighting::GROUPS_CLUSTER.id => {
                Handler::read(&groups::HandlerAdaptor(&runtime.groups), ctx, reply)
            }
            id if id == lighting::SCENES_CLUSTER.id => self.read_scenes(runtime, ctx, reply).await,
            id if id == rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id => {
                Handler::read(
                    &rs_matter::dm::clusters::decl::color_control::HandlerAdaptor(
                        runtime.lighting(),
                    ),
                    ctx,
                    reply,
                )
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    async fn read_scenes(
        &self,
        runtime: &FeatureRuntime,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> Result<(), Error> {
        use rs_matter::dm::clusters::decl::scenes_management::AttributeId;

        if ctx.attr().attr_id == AttributeId::FabricSceneInfo as u32 {
            let fabric = ctx.accessor()?.fab_idx()?;
            if runtime.lighting().pending_scene(fabric).is_some() {
                return self
                    .read_scenes_inner(runtime, ctx, SceneValidReadReply(reply))
                    .await;
            }
        }
        self.read_scenes_inner(runtime, ctx, reply).await
    }

    async fn read_scenes_inner(
        &self,
        runtime: &FeatureRuntime,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> Result<(), Error> {
        let scoped = SceneContext::new(&ctx, self.store.clone(), runtime.allocation.endpoint);
        let dataver = Dataver::new(runtime.scenes_dataver.get());
        let has_level = runtime
            .clusters
            .iter()
            .any(|cluster| cluster.id == lighting::LEVEL_CLUSTER.id);
        let has_color = runtime.clusters.iter().any(|cluster| {
            cluster.id == rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id
        });
        if has_level && has_color {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (
                        lighting::SceneLevel(runtime.lighting()),
                        (lighting::SceneColor(runtime.lighting()), ()),
                    ),
                ),
            );
            AsyncHandler::read(&handler.adapt(), &scoped, reply).await
        } else if has_level {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (lighting::SceneLevel(runtime.lighting()), ()),
                ),
            );
            AsyncHandler::read(&handler.adapt(), &scoped, reply).await
        } else if has_color {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (lighting::SceneColor(runtime.lighting()), ()),
                ),
            );
            AsyncHandler::read(&handler.adapt(), &scoped, reply).await
        } else {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (lighting::SceneOnOff(runtime.lighting()), ()),
            );
            AsyncHandler::read(&handler.adapt(), &scoped, reply).await
        }
    }

    fn dispatch_write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let endpoint = ctx.endpt().ok_or(ErrorCode::AttributeNotFound)?;
        let cluster = ctx.cluster().ok_or(ErrorCode::AttributeNotFound)?;
        let runtimes = self.snapshot();
        let runtime = runtimes
            .iter()
            .find(|runtime| runtime.allocation.endpoint == endpoint)
            .ok_or(ErrorCode::AttributeNotFound)?;
        if runtime
            .clusters
            .iter()
            .find(|candidate| candidate.id == cluster)
            .and_then(|candidate| candidate.attribute(ctx.attr().attr_id))
            .is_none()
        {
            return Err(ErrorCode::AttributeNotFound.into());
        }
        match cluster {
            id if id == identify::IdentifyHandler::<()>::CLUSTER.id => {
                Handler::write(&identify::HandlerAdaptor(&runtime.identify), ctx)
            }
            id if id == common::BRIDGED_CLUSTER.id => {
                Handler::write(&bridged::HandlerAdaptor(&runtime.common), ctx)
            }
            id if id == lighting::ON_OFF_CLUSTER.id => Handler::write(
                &rs_matter::dm::clusters::decl::on_off::HandlerAdaptor(runtime.lighting()),
                ctx,
            ),
            id if id == lighting::LEVEL_CLUSTER.id => Handler::write(
                &rs_matter::dm::clusters::decl::level_control::HandlerAdaptor(runtime.lighting()),
                ctx,
            ),
            id if id == rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id => {
                Handler::write(
                    &rs_matter::dm::clusters::decl::color_control::HandlerAdaptor(
                        runtime.lighting(),
                    ),
                    ctx,
                )
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    async fn dispatch_invoke(
        &self,
        ctx: impl InvokeContext,
        reply: impl InvokeReply,
    ) -> Result<(), Error> {
        let endpoint = ctx.endpt().ok_or(ErrorCode::AttributeNotFound)?;
        let cluster = ctx.cluster().ok_or(ErrorCode::AttributeNotFound)?;
        let runtimes = self.snapshot();
        let runtime = runtimes
            .iter()
            .find(|runtime| runtime.allocation.endpoint == endpoint)
            .ok_or(ErrorCode::AttributeNotFound)?;
        if runtime
            .clusters
            .iter()
            .find(|candidate| candidate.id == cluster)
            .and_then(|candidate| candidate.command(ctx.cmd().cmd_id))
            .is_none()
        {
            return Err(ErrorCode::CommandNotFound.into());
        }
        match cluster {
            id if id == identify::IdentifyHandler::<()>::CLUSTER.id => {
                Handler::invoke(&identify::HandlerAdaptor(&runtime.identify), ctx, reply)
            }
            id if id == lighting::GROUPS_CLUSTER.id => {
                let handler = groups::GroupsHandler::new_with_identify(
                    Dataver::new(groups::ClusterHandler::dataver(&runtime.groups)),
                    &runtime.identify,
                );
                let result = Handler::invoke(&groups::HandlerAdaptor(&handler), ctx, reply);
                if result.is_ok() {
                    groups::ClusterHandler::dataver_changed(&runtime.groups);
                }
                result
            }
            id if id == lighting::SCENES_CLUSTER.id => {
                self.invoke_scenes(runtime, ctx, reply).await
            }
            id if id == lighting::ON_OFF_CLUSTER.id => runtime.lighting().invoke_on_off(ctx).await,
            id if id == lighting::LEVEL_CLUSTER.id => {
                let lighting = runtime.lighting();
                let was_active = lighting.adjustment_active();
                let result = lighting.invoke_level(&ctx).await;
                if result.is_ok() {
                    lighting.adjustment_command_completed(&ctx, was_active);
                }
                result
            }
            id if id == rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id => {
                let lighting = runtime.lighting();
                let was_active = lighting.adjustment_active();
                let result = lighting.invoke_color(&ctx).await;
                if result.is_ok() {
                    lighting.adjustment_command_completed(&ctx, was_active);
                }
                result
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }

    async fn invoke_scenes(
        &self,
        runtime: &FeatureRuntime,
        ctx: impl InvokeContext,
        reply: impl InvokeReply,
    ) -> Result<(), Error> {
        use rs_matter::dm::clusters::decl::scenes_management::CommandId;
        let recall = ctx.cmd().cmd_id == CommandId::RecallScene as u32;
        if recall {
            use rs_matter::dm::clusters::scenes::SceneInvalidator as _;

            let request = rs_matter::dm::clusters::decl::scenes_management::RecallSceneRequest::new(
                ctx.data().clone(),
            );
            runtime
                .scenes
                .scenable_attribute_changed(runtime.allocation.endpoint);
            runtime.lighting().begin_scene_recall(
                ctx.accessor()?.fab_idx()?,
                request.group_id()?,
                request.scene_id()?,
            );
        }
        let scoped = SceneContext::new(&ctx, self.store.clone(), runtime.allocation.endpoint);
        let dataver = Dataver::new(runtime.scenes_dataver.get());
        let has_level = runtime
            .clusters
            .iter()
            .any(|cluster| cluster.id == lighting::LEVEL_CLUSTER.id);
        let has_color = runtime.clusters.iter().any(|cluster| {
            cluster.id == rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id
        });
        let result = if has_level && has_color {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (
                        lighting::SceneLevel(runtime.lighting()),
                        (lighting::SceneColor(runtime.lighting()), ()),
                    ),
                ),
            );
            AsyncHandler::invoke(&handler.adapt(), &scoped, reply).await
        } else if has_level {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (lighting::SceneLevel(runtime.lighting()), ()),
                ),
            );
            AsyncHandler::invoke(&handler.adapt(), &scoped, reply).await
        } else if has_color {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (
                    lighting::SceneOnOff(runtime.lighting()),
                    (lighting::SceneColor(runtime.lighting()), ()),
                ),
            );
            AsyncHandler::invoke(&handler.adapt(), &scoped, reply).await
        } else {
            let handler = scenes::ScenesHandler::new(
                dataver,
                &runtime.scenes,
                (lighting::SceneOnOff(runtime.lighting()), ()),
            );
            AsyncHandler::invoke(&handler.adapt(), &scoped, reply).await
        };
        if recall {
            let scene_change = runtime.lighting().finish_scene_recall(result.is_ok());
            if scene_change != 0 {
                ctx.notify_attr_changed(
                    runtime.allocation.endpoint,
                    lighting::SCENES_CLUSTER.id,
                    rs_matter::dm::clusters::decl::scenes_management::AttributeId::FabricSceneInfo
                        as _,
                );
            }
        }
        if result.is_ok() {
            runtime.scenes_dataver.changed();
        }
        result
    }

    fn process_state_change(
        &self,
        ctx: &impl HandlerContext,
        change: DeviceChange,
    ) -> Result<(), Error> {
        match change {
            DeviceChange::AvailabilityChanged { feature, available } => {
                self.report_reachability(ctx, &feature, available)?;
            }
            DeviceChange::StateChanged {
                feature,
                properties,
            } => {
                let Some(endpoint) = self.endpoint_for(&feature) else {
                    return Ok(());
                };
                let runtimes = self.snapshot();
                let runtime = runtimes
                    .iter()
                    .find(|runtime| runtime.allocation.feature == feature)
                    .unwrap();
                for property in properties {
                    if !runtime.exposed_values.borrow().contains_key(&property) {
                        continue;
                    }
                    let current = self
                        .service
                        .snapshot(&feature)
                        .and_then(|snapshot| snapshot.property(property).cloned())
                        .and_then(|state| match state {
                            crate::device::PropertyState::Current { value, .. } => Some(value),
                            _ => None,
                        });
                    let mut exposed = runtime.exposed_values.borrow_mut();
                    if exposed.get(&property) == Some(&current) {
                        continue;
                    }
                    let previous = exposed.insert(property, current.clone()).flatten();
                    if matches!(
                        property,
                        Property::Power
                            | Property::Brightness
                            | Property::ColorTemperature
                            | Property::Color
                    ) {
                        use rs_matter::dm::clusters::scenes::SceneInvalidator as _;
                        let scene_change = runtime.lighting().scene_state_changed();
                        if scene_change < 0 {
                            runtime.scenes.scenable_attribute_changed(endpoint);
                        }
                        if scene_change != 0 {
                            ctx.notify_attr_changed(
                                endpoint,
                                lighting::SCENES_CLUSTER.id,
                                rs_matter::dm::clusters::decl::scenes_management::AttributeId::FabricSceneInfo as _,
                            );
                        }
                    }
                    let report_property = property != Property::Brightness
                        || runtime
                            .lighting()
                            .should_report_brightness(previous.as_ref(), current.as_ref());
                    if report_property {
                        for (cluster, attribute) in property_paths(property) {
                            ctx.notify_attr_changed(endpoint, cluster, attribute);
                        }
                    }
                }
            }
            DeviceChange::FeatureUpdated(_)
            | DeviceChange::FeaturePublished(_)
            | DeviceChange::FeatureRemoved(_)
            | DeviceChange::Resync => {}
        }
        Ok(())
    }

    fn reconcile(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        let loaded = self.store.record(self.devices.load_features(true))?;
        let allocations = loaded
            .into_iter()
            .filter(|allocation| allocation.active)
            .map(|allocation| (allocation.feature.clone(), allocation))
            .collect::<BTreeMap<_, _>>();
        let mut desired = Vec::new();
        for published in self.service.features() {
            let Some(allocation) = allocations.get(&published.identity).cloned() else {
                continue;
            };
            if let Some(runtime) = self
                .build_runtime(allocation, published.name, published.capabilities.0)
                .map_err(|_| ErrorCode::Failure)?
            {
                desired.push(Rc::new(runtime));
            }
        }
        desired.sort_by_key(|runtime| runtime.allocation.endpoint);
        let current = self.snapshot();
        let shape_changed = desired.iter().any(|candidate| {
            current
                .iter()
                .find(|runtime| runtime.allocation.feature == candidate.allocation.feature)
                .is_some_and(|runtime| runtime.shape_signature != candidate.shape_signature)
        });
        if shape_changed {
            *self.requested_topology_signature.borrow_mut() =
                Some(topology_signature_for(&desired));
            self.rebuild_requested.set(true);
            self.rebuild_event.notify(usize::MAX);
            self.topology_dirty.set(true);
            return self.finish_topology_change(ctx);
        }

        *self.requested_topology_signature.borrow_mut() = None;
        let mut updated = Vec::with_capacity(desired.len());
        for candidate in desired {
            let Some(runtime) = current
                .iter()
                .find(|runtime| runtime.allocation.feature == candidate.allocation.feature)
            else {
                updated.push(candidate);
                self.topology_dirty.set(true);
                continue;
            };
            self.report_reachability(
                ctx,
                &runtime.allocation.feature,
                self.service.is_available(&runtime.allocation.feature),
            )?;
            if runtime
                .common
                .set_default_label(&candidate.common.default_label())
            {
                ctx.notify_attr_changed(
                    runtime.allocation.endpoint,
                    common::BRIDGED_CLUSTER.id,
                    bridged::AttributeId::NodeLabel as _,
                );
            }
            let config_changed =
                *candidate.config_signature.borrow() != *runtime.config_signature.borrow();
            let capabilities = candidate.sensor.capabilities();
            let capabilities_changed = capabilities != runtime.sensor.capabilities();
            runtime.sensor.set_capabilities(capabilities.clone());
            if let Some(lighting) = &runtime.lighting {
                lighting.set_capabilities(capabilities.clone());
            }
            for property in exposed_properties(&capabilities, &runtime.clusters) {
                let current = self
                    .service
                    .snapshot(&runtime.allocation.feature)
                    .and_then(|snapshot| snapshot.property(property).cloned())
                    .and_then(|state| match state {
                        crate::device::PropertyState::Current { value, .. } => Some(value),
                        _ => None,
                    });
                let mut exposed = runtime.exposed_values.borrow_mut();
                if exposed.get(&property) != Some(&current) {
                    exposed.insert(property, current);
                    for (cluster, attribute) in property_paths(property) {
                        ctx.notify_attr_changed(runtime.allocation.endpoint, cluster, attribute);
                    }
                }
            }
            if capabilities_changed {
                self.notify_sensor_configuration(ctx, runtime);
            }
            if config_changed {
                *runtime.config_signature.borrow_mut() =
                    candidate.config_signature.borrow().clone();
                self.topology_dirty.set(true);
            }
            updated.push(runtime.clone());
        }
        if updated.len() != current.len() {
            self.topology_dirty.set(true);
        }
        *self.runtimes.borrow_mut() = Rc::new(updated);
        self.finish_topology_change(ctx)
    }

    fn notify_sensor_configuration(&self, ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
        let endpoint = runtime.allocation.endpoint;
        for cluster in &runtime.clusters {
            match cluster.id {
                id if id == sensors::TEMPERATURE_CLUSTER.id => {
                    for attribute in [
                        temperature_measurement::AttributeId::MeasuredValue,
                        temperature_measurement::AttributeId::MinMeasuredValue,
                        temperature_measurement::AttributeId::MaxMeasuredValue,
                    ] {
                        ctx.notify_attr_changed(endpoint, id, attribute as _);
                    }
                }
                id if id == sensors::HUMIDITY_CLUSTER.id => {
                    for attribute in [
                        relative_humidity_measurement::AttributeId::MeasuredValue,
                        relative_humidity_measurement::AttributeId::MinMeasuredValue,
                        relative_humidity_measurement::AttributeId::MaxMeasuredValue,
                    ] {
                        ctx.notify_attr_changed(endpoint, id, attribute as _);
                    }
                }
                id if id == sensors::ILLUMINANCE_CLUSTER.id => {
                    for attribute in [
                        illuminance_measurement::AttributeId::MeasuredValue,
                        illuminance_measurement::AttributeId::MinMeasuredValue,
                        illuminance_measurement::AttributeId::MaxMeasuredValue,
                    ] {
                        ctx.notify_attr_changed(endpoint, id, attribute as _);
                    }
                }
                _ => {}
            }
        }
    }

    fn finish_topology_change(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        if self.topology_dirty.replace(false) {
            self.persist_configuration_change(ctx)?;
            ctx.notify_attr_changed(
                0,
                desc::DescHandler::CLUSTER.id,
                desc::AttributeId::PartsList as _,
            );
            ctx.notify_attr_changed(
                1,
                desc::DescHandler::CLUSTER.id,
                desc::AttributeId::PartsList as _,
            );
        }
        Ok(())
    }

    fn report_reachability(
        &self,
        ctx: &impl HandlerContext,
        feature: &FeatureIdentity,
        available: bool,
    ) -> Result<(), Error> {
        let snapshot = self.snapshot();
        let Some(runtime) = snapshot
            .iter()
            .find(|runtime| &runtime.allocation.feature == feature)
        else {
            return Ok(());
        };
        if runtime.reachable.replace(available) == available {
            return Ok(());
        }
        let endpoint = runtime.allocation.endpoint;
        ctx.notify_attr_changed(
            endpoint,
            common::BRIDGED_CLUSTER.id,
            bridged::AttributeId::Reachable as _,
        );
        bridged::ReachableChanged::emit_for(ctx, endpoint, |builder| {
            builder.reachable_new_value(available)?.end()
        })?;
        Ok(())
    }

    fn persist_configuration_change(&self, ctx: &impl HandlerContext) -> Result<(), Error> {
        let signature = self
            .requested_topology_signature
            .borrow()
            .clone()
            .unwrap_or_else(|| self.topology_signature());
        let stored_signature = self
            .store
            .record(self.store.storage().topology_signature())?;
        if stored_signature.is_empty() {
            let bytes = self
                .store
                .record(self.store.storage().get(BASIC_INFO_KEY))?
                .ok_or(ErrorCode::InvalidData)?;
            self.store.record(self.store.storage().save_topology(
                BASIC_INFO_KEY,
                &bytes,
                &signature,
            ))?;
            return Ok(());
        }
        if stored_signature == signature {
            return Ok(());
        }
        ctx.matter().bump_configuration_version(
            ctx.matter()
                .kv(TopologyStore::new(self.store.clone(), signature)),
            ctx,
        )?;
        Ok(())
    }
}

/// Executable Matter bridge that preserves the live transport across model rebuilds.
pub struct DeviceBridge<'a> {
    service: &'a DeviceService,
    identity: &'a Identity,
    devices: DeviceStore,
    store: StoreAdapter,
}

impl<'a> DeviceBridge<'a> {
    pub fn new(service: &'a DeviceService, identity: &'a Identity, store: Store) -> Self {
        Self {
            service,
            identity,
            devices: store.devices(),
            store: StoreAdapter::new(store.matter()),
        }
    }

    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }

    pub async fn run(
        &self,
        port: u16,
        report_pairing: impl Fn(super::PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.check_failure()?;
        let mut bind_addr = MATTER_SOCKET_BIND_ADDR;
        bind_addr.set_port(port);
        let socket = async_io::Async::<UdpSocket>::bind(bind_addr)
            .map_err(|error| format!("Failed to bind Matter UDP port {port}: {error}"))?;
        let bound_port = socket.get_ref().local_addr()?.port();
        let info = model::basic_info(self.identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, bound_port);
        let kv = matter.kv(self.store.clone());
        self.store
            .with_context("restore Matter data", matter.startup(&kv))?;
        let missing_basic_info = !self.store.storage().contains(BASIC_INFO_KEY)?;
        model::initialize_basic_info(&matter, &kv, missing_basic_info).map_err(|error| {
            StorageError::new(
                self.store.storage().path(),
                "initialize Matter basic information",
                error,
            )
        })?;
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let opened = !matter.has_fabrics();
        if opened {
            matter.open_basic_comm_window(WINDOW_SECONDS, &crypto, &())?;
            report_pairing(super::pairing::opened(&info, WINDOW_SECONDS)?)?;
        }
        let timeout = async {
            if opened {
                async_io::Timer::after(Duration::from_secs(WINDOW_SECONDS.into())).await;
                if !matter.has_fabrics() {
                    report_pairing(super::PairingEvent::Expired)?;
                }
            }
            std::future::pending::<Result<(), RuntimeError>>().await
        };
        let fatal = async { Err::<(), RuntimeError>(self.store.wait_failure().await.into()) };
        let transport = async {
            matter
                .run(&crypto, &socket, &socket, &socket)
                .await
                .map_err(|error| format!("Matter transport task failed: {error}").into())
        };
        let mdns = async {
            AstroMdns::new()
                .run(&matter)
                .await
                .map_err(|error| format!("mDNS service failed: {error}").into())
        };
        let protocol = async {
            loop {
                let model = DeviceBridgeModel::with_store(
                    self.service.clone(),
                    self.devices.clone(),
                    self.store.clone(),
                )?;
                let mut random = crypto.rand()?;
                let handler = endpoints::EthSysHandlerBuilder::new()
                    .netif_diag(&SysNetifs)
                    .build(&mut random)
                    .chain(|endpoint, _| endpoint != 0, &model);
                let im = InteractionModel::new(
                    &matter,
                    &crypto,
                    &buffers,
                    (&model, &handler),
                    &kv,
                    &state,
                );
                self.store
                    .with_context("restore Matter model", im.startup().await)?;
                let responder = DefaultResponder::new(&im);
                let outcome = future::or(
                    async { responder.run::<4, 4>().await.map(|_| false) },
                    future::or(async { im.run().await.map(|_| false) }, async {
                        model.rebuild_requested().await;
                        Ok(true)
                    }),
                )
                .await
                .map_err(|error| -> RuntimeError {
                    format!("Matter model task failed: {error}").into()
                })?;
                if !outcome {
                    return Err("Matter model stopped unexpectedly".into());
                }
            }
        };
        log::info!("Matter UDP listening on port {bound_port}");
        let result = future::or(
            fatal,
            future::or(timeout, future::or(transport, future::or(mdns, protocol))),
        )
        .await;
        self.check_failure()?;
        result
    }
}

impl Metadata for DeviceBridgeModel {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let runtimes = self.snapshot();
        let mut endpoints = Vec::with_capacity(runtimes.len() + 2);
        endpoints.push(model::NODE.endpoints[0].clone());
        endpoints.push(model::NODE.endpoints[1].clone());
        for runtime in runtimes.iter() {
            endpoints.push(
                Endpoint::new(
                    runtime.allocation.endpoint,
                    &runtime.device_types,
                    &runtime.clusters,
                )
                .with_unique_id(runtime.allocation.public_id.as_str()),
            );
        }
        f(&Node::new(&endpoints))
    }
}

impl AsyncHandler for DeviceBridgeModel {
    fn read_awaits(&self, _ctx: impl ReadContext) -> bool {
        true
    }
    fn write_awaits(&self, _ctx: impl WriteContext) -> bool {
        false
    }
    fn invoke_awaits(&self, _ctx: impl InvokeContext) -> bool {
        true
    }
    fn read(
        &self,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> impl Future<Output = Result<(), Error>> {
        self.dispatch_read(ctx, reply)
    }
    fn write(&self, ctx: impl WriteContext) -> impl Future<Output = Result<(), Error>> {
        future::ready(self.dispatch_write(ctx))
    }
    fn invoke(
        &self,
        ctx: impl InvokeContext,
        reply: impl InvokeReply,
    ) -> impl Future<Output = Result<(), Error>> {
        self.dispatch_invoke(ctx, reply)
    }
    fn bump_dataver(&self, ctx: impl MatchContext) {
        if ctx.endpt().is_none_or(|endpoint| endpoint == 1)
            && ctx
                .cluster()
                .is_none_or(|cluster| cluster == desc::DescHandler::CLUSTER.id)
        {
            desc::ClusterHandler::dataver_changed(&self.aggregator_desc);
        }
        let runtimes = self.snapshot();
        for runtime in runtimes.iter().filter(|runtime| {
            ctx.endpt()
                .is_none_or(|endpoint| endpoint == runtime.allocation.endpoint)
        }) {
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == desc::DescHandler::CLUSTER.id)
            {
                desc::ClusterHandler::dataver_changed(&runtime.desc);
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == common::BRIDGED_CLUSTER.id)
            {
                bridged::ClusterHandler::dataver_changed(&runtime.common);
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == identify::IdentifyHandler::<()>::CLUSTER.id)
            {
                identify::ClusterHandler::dataver_changed(&runtime.identify);
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == lighting::GROUPS_CLUSTER.id)
            {
                groups::ClusterHandler::dataver_changed(&runtime.groups);
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == lighting::SCENES_CLUSTER.id)
            {
                runtime.scenes_dataver.changed();
            }
            if let Some(cluster) = ctx.cluster() {
                if let Some(dataver) = runtime
                    .lighting
                    .as_ref()
                    .and_then(|lighting| lighting.dataver_for(cluster))
                {
                    dataver.changed();
                }
            } else {
                for cluster in &runtime.clusters {
                    if let Some(dataver) = runtime
                        .lighting
                        .as_ref()
                        .and_then(|lighting| lighting.dataver_for(cluster.id))
                    {
                        dataver.changed();
                    }
                }
            }
            if let Some(cluster) = ctx.cluster() {
                if let Some(dataver) = runtime.sensor.dataver(cluster) {
                    dataver.changed();
                }
            } else {
                for cluster in &runtime.clusters {
                    if let Some(dataver) = runtime.sensor.dataver(cluster.id) {
                        dataver.changed();
                    }
                }
            }
        }
    }
    fn lifecycle(
        &self,
        ctx: impl HandlerContext,
        op: rs_matter::dm::LifecycleOp,
    ) -> Result<(), Error> {
        use rs_matter::dm::clusters::decl::scenes_management::ClusterAsyncHandler as _;
        use rs_matter::dm::clusters::scenes::SceneInvalidator as _;

        for runtime in self.snapshot().iter() {
            groups::ClusterHandler::lifecycle(&runtime.groups, &ctx, op)?;
            if runtime
                .clusters
                .iter()
                .any(|cluster| cluster.id == lighting::SCENES_CLUSTER.id)
            {
                let scoped =
                    SceneContext::new(&ctx, self.store.clone(), runtime.allocation.endpoint);
                let handler = scenes::ScenesHandler::<33, (), 96>::new(
                    Dataver::new(runtime.scenes_dataver.get()),
                    &runtime.scenes,
                    (),
                );
                handler.lifecycle(&scoped, op)?;
                if matches!(op, rs_matter::dm::LifecycleOp::Startup) {
                    runtime
                        .scenes
                        .scenable_attribute_changed(runtime.allocation.endpoint);
                }
            }
        }
        Ok(())
    }
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut subscription = self.service.subscribe();
        self.reconcile(&ctx)?;
        let mut background_runtimes = self.snapshot();
        let mut background = Box::pin(Self::run_feature_backgrounds(
            background_runtimes.clone(),
            &ctx,
        ));
        loop {
            enum Next {
                Changes(Vec<DeviceChange>),
                BackgroundStopped(Result<(), Error>),
            }
            let next = future::or(
                async { Next::Changes(subscription.changed().await) },
                async { Next::BackgroundStopped(background.as_mut().await) },
            )
            .await;
            let changes = match next {
                Next::Changes(changes) => changes,
                Next::BackgroundStopped(result) => return result,
            };
            let reconcile = changes.iter().any(|change| {
                matches!(
                    change,
                    DeviceChange::FeaturePublished(_)
                        | DeviceChange::FeatureUpdated(_)
                        | DeviceChange::FeatureRemoved(_)
                        | DeviceChange::Resync
                )
            });
            for change in changes {
                self.process_state_change(&ctx, change)?;
            }
            if reconcile {
                self.reconcile(&ctx)?;
                let latest = self.snapshot();
                let same_runtimes = latest.len() == background_runtimes.len()
                    && latest
                        .iter()
                        .zip(background_runtimes.iter())
                        .all(|(left, right)| Rc::ptr_eq(left, right));
                if !same_runtimes {
                    drop(background);
                    background_runtimes = latest;
                    background = Box::pin(Self::run_feature_backgrounds(
                        background_runtimes.clone(),
                        &ctx,
                    ));
                }
            }
        }
    }
}

fn property_paths(property: Property) -> Vec<(u32, u32)> {
    let single = |path| vec![path];
    match property {
        Property::Power => single((
            lighting::ON_OFF_CLUSTER.id,
            rs_matter::dm::clusters::decl::on_off::AttributeId::OnOff as _,
        )),
        Property::Brightness => single((
            lighting::LEVEL_CLUSTER.id,
            rs_matter::dm::clusters::decl::level_control::AttributeId::CurrentLevel as _,
        )),
        Property::ColorTemperature => single((
            rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id,
            rs_matter::dm::clusters::decl::color_control::AttributeId::ColorTemperatureMireds as _,
        )),
        Property::Color => vec![
            (
                rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id,
                rs_matter::dm::clusters::decl::color_control::AttributeId::CurrentX as _,
            ),
            (
                rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id,
                rs_matter::dm::clusters::decl::color_control::AttributeId::CurrentY as _,
            ),
        ],
        Property::Temperature => single((
            sensors::TEMPERATURE_CLUSTER.id,
            temperature_measurement::AttributeId::MeasuredValue as _,
        )),
        Property::Humidity => single((
            sensors::HUMIDITY_CLUSTER.id,
            relative_humidity_measurement::AttributeId::MeasuredValue as _,
        )),
        Property::Illuminance => single((
            sensors::ILLUMINANCE_CLUSTER.id,
            illuminance_measurement::AttributeId::MeasuredValue as _,
        )),
        Property::Motion | Property::Occupancy => single((
            occupancy_sensing::FULL_CLUSTER.id,
            occupancy_sensing::AttributeId::Occupancy as _,
        )),
        Property::Contact => single((
            sensors::BOOLEAN_STATE_CLUSTER.id,
            boolean_state::AttributeId::StateValue as _,
        )),
        Property::Battery => single((
            sensors::POWER_SOURCE_CLUSTER.id,
            power_source::AttributeId::BatPercentRemaining as _,
        )),
        _ => Vec::new(),
    }
}

fn shape_signature(device_types: &[DeviceType], clusters: &[Cluster<'_>]) -> String {
    let mut digest = Sha1::new();
    digest.update(b"device-types");
    digest.update((device_types.len() as u32).to_be_bytes());
    for device_type in device_types {
        digest.update(device_type.dtype.to_be_bytes());
        digest.update(device_type.drev.to_be_bytes());
    }
    for cluster in clusters {
        digest.update(b"cluster");
        digest.update(cluster.id.to_be_bytes());
        digest.update(cluster.revision.to_be_bytes());
        digest.update(cluster.feature_map.to_be_bytes());
        let attributes = cluster
            .attributes
            .iter()
            .filter(|attribute| {
                (cluster.with_attrs)(attribute, cluster.revision, cluster.feature_map)
            })
            .collect::<Vec<_>>();
        digest.update(b"attributes");
        digest.update((attributes.len() as u32).to_be_bytes());
        for attribute in attributes {
            digest.update(attribute.id.to_be_bytes());
        }
        let commands = cluster
            .commands
            .iter()
            .filter(|command| (cluster.with_cmds)(command, cluster.revision, cluster.feature_map))
            .collect::<Vec<_>>();
        digest.update(b"commands");
        digest.update((commands.len() as u32).to_be_bytes());
        for command in commands {
            digest.update(command.id.to_be_bytes());
        }
        let events = cluster
            .events
            .iter()
            .filter(|event| (cluster.with_events)(event, cluster.revision, cluster.feature_map))
            .collect::<Vec<_>>();
        digest.update(b"events");
        digest.update((events.len() as u32).to_be_bytes());
        for event in events {
            digest.update(event.id.to_be_bytes());
        }
    }
    hex_digest(digest.finalize().as_slice())
}

pub(super) fn config_signature(shape: &str, capabilities: &[Capability]) -> String {
    let mut digest = Sha1::new();
    digest.update(shape.as_bytes());
    for capability in capabilities {
        match capability {
            Capability::Temperature(range) => {
                digest.update(b"temperature");
                if let Some((minimum, maximum)) = sensors::temperature_bounds(Some(*range)) {
                    digest.update(minimum.to_be_bytes());
                    digest.update(maximum.to_be_bytes());
                }
            }
            Capability::Humidity(range) => {
                digest.update(b"humidity");
                if let Some((minimum, maximum)) = sensors::humidity_bounds(Some(*range)) {
                    digest.update(minimum.to_be_bytes());
                    digest.update(maximum.to_be_bytes());
                }
            }
            Capability::Illuminance(range) => {
                digest.update(b"illuminance");
                if let Some((minimum, maximum)) = sensors::illuminance_bounds(Some(*range)) {
                    digest.update(minimum.to_be_bytes());
                    digest.update(maximum.to_be_bytes());
                }
            }
            _ => {}
        }
    }
    hex_digest(digest.finalize().as_slice())
}

fn topology_signature_for(runtimes: &[Rc<FeatureRuntime>]) -> String {
    let mut digest = Sha1::new();
    for runtime in runtimes {
        digest.update(runtime.allocation.endpoint.to_be_bytes());
        digest.update(runtime.shape_signature.as_bytes());
        digest.update(runtime.config_signature.borrow().as_bytes());
    }
    hex_digest(digest.finalize().as_slice())
}

fn exposed_properties(capabilities: &[Capability], clusters: &[Cluster<'_>]) -> Vec<Property> {
    capabilities
        .iter()
        .filter_map(|capability| {
            let (property, cluster) = match capability {
                Capability::Temperature(_) => {
                    (Property::Temperature, sensors::TEMPERATURE_CLUSTER.id)
                }
                Capability::Humidity(_) => (Property::Humidity, sensors::HUMIDITY_CLUSTER.id),
                Capability::Illuminance(_) => {
                    (Property::Illuminance, sensors::ILLUMINANCE_CLUSTER.id)
                }
                Capability::Motion => (Property::Motion, occupancy_sensing::FULL_CLUSTER.id),
                Capability::Occupancy => (Property::Occupancy, occupancy_sensing::FULL_CLUSTER.id),
                Capability::Contact => (Property::Contact, sensors::BOOLEAN_STATE_CLUSTER.id),
                Capability::Battery(_) => (Property::Battery, sensors::POWER_SOURCE_CLUSTER.id),
                Capability::Power { .. } => (Property::Power, lighting::ON_OFF_CLUSTER.id),
                Capability::Brightness(_) => (Property::Brightness, lighting::LEVEL_CLUSTER.id),
                Capability::ColorTemperature(_) => (
                    Property::ColorTemperature,
                    rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id,
                ),
                Capability::Color => (
                    Property::Color,
                    rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id,
                ),
                _ => return None,
            };
            clusters
                .iter()
                .any(|item| item.id == cluster)
                .then_some(property)
        })
        .collect()
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
