use super::{
    common, model, sensors,
    storage::{StoreAdapter, TopologyStore},
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
        AsyncHandler, Cluster, Dataver, DeviceType, Endpoint, Handler, HandlerContext,
        InvokeContext, InvokeReply, MatchContext, Metadata, Node, ReadContext, ReadReply,
        WriteContext,
        clusters::decl::{
            boolean_state, bridged_device_basic_information as bridged, illuminance_measurement,
            occupancy_sensing, power_source, relative_humidity_measurement,
            temperature_measurement,
        },
        clusters::{desc, identify},
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        devices::{DEV_TYPE_BRIDGED_NODE, DEV_TYPE_POWER_SOURCE},
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    error::{Error, ErrorCode},
    im::{EthInteractionModelState, InteractionModel},
    persist::BASIC_INFO_KEY,
    respond::DefaultResponder,
    transport::{
        MATTER_SOCKET_BIND_ADDR, exchange::MatterBuffers, network::mdns::astro::AstroMdns,
    },
};
use sha1::{Digest, Sha1};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    net::UdpSocket,
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
            FeatureRole::TemperatureSensor
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
        if !(has_temperature || has_humidity || has_lux || has_occupancy || has_contact) {
            return Ok(None);
        }
        clusters.push(identify::IdentifyHandler::<()>::CLUSTER);
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
        let exposed_values = sensor_properties(&capabilities, &clusters)
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
                capabilities,
                allocation.endpoint,
                seed.wrapping_add(3),
            ),
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

    fn dispatch_read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
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
            _ => Err(ErrorCode::AttributeNotFound.into()),
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
        match cluster {
            id if id == identify::IdentifyHandler::<()>::CLUSTER.id => {
                Handler::write(&identify::HandlerAdaptor(&runtime.identify), ctx)
            }
            id if id == common::BRIDGED_CLUSTER.id => {
                Handler::write(&bridged::HandlerAdaptor(&runtime.common), ctx)
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn dispatch_invoke(
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
        match cluster {
            id if id == identify::IdentifyHandler::<()>::CLUSTER.id => {
                Handler::invoke(&identify::HandlerAdaptor(&runtime.identify), ctx, reply)
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
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
                    exposed.insert(property, current);
                    if let Some((cluster, attribute)) = sensor_path(property) {
                        ctx.notify_attr_changed(endpoint, cluster, attribute);
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
            for property in sensor_properties(&capabilities, &runtime.clusters) {
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
                    if let Some((cluster, attribute)) = sensor_path(property) {
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
        false
    }
    fn write_awaits(&self, _ctx: impl WriteContext) -> bool {
        false
    }
    fn invoke_awaits(&self, _ctx: impl InvokeContext) -> bool {
        false
    }
    fn read(
        &self,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> impl Future<Output = Result<(), Error>> {
        future::ready(self.dispatch_read(ctx, reply))
    }
    fn write(&self, ctx: impl WriteContext) -> impl Future<Output = Result<(), Error>> {
        future::ready(self.dispatch_write(ctx))
    }
    fn invoke(
        &self,
        ctx: impl InvokeContext,
        reply: impl InvokeReply,
    ) -> impl Future<Output = Result<(), Error>> {
        future::ready(self.dispatch_invoke(ctx, reply))
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
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        let mut subscription = self.service.subscribe();
        self.reconcile(&ctx)?;
        loop {
            let snapshot = self.snapshot();
            enum Next {
                Changes(Vec<DeviceChange>),
                IdentifyStopped(Result<(), Error>),
            }
            let next = future::or(
                async { Next::Changes(subscription.changed().await) },
                async {
                    let futures = snapshot
                        .iter()
                        .map(|runtime| {
                            Box::pin(identify::ClusterHandler::run(&runtime.identify, &ctx))
                                as std::pin::Pin<Box<dyn Future<Output = Result<(), Error>> + '_>>
                        })
                        .collect::<Vec<_>>();
                    if futures.is_empty() {
                        std::future::pending::<Next>().await
                    } else {
                        let (result, _, _) = futures_util::future::select_all(futures).await;
                        Next::IdentifyStopped(result)
                    }
                },
            )
            .await;
            let changes = match next {
                Next::Changes(changes) => changes,
                Next::IdentifyStopped(result) => return result,
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
            }
        }
    }
}

fn sensor_path(property: Property) -> Option<(u32, u32)> {
    Some(match property {
        Property::Temperature => (
            sensors::TEMPERATURE_CLUSTER.id,
            temperature_measurement::AttributeId::MeasuredValue as _,
        ),
        Property::Humidity => (
            sensors::HUMIDITY_CLUSTER.id,
            relative_humidity_measurement::AttributeId::MeasuredValue as _,
        ),
        Property::Illuminance => (
            sensors::ILLUMINANCE_CLUSTER.id,
            illuminance_measurement::AttributeId::MeasuredValue as _,
        ),
        Property::Motion | Property::Occupancy => (
            occupancy_sensing::FULL_CLUSTER.id,
            occupancy_sensing::AttributeId::Occupancy as _,
        ),
        Property::Contact => (
            sensors::BOOLEAN_STATE_CLUSTER.id,
            boolean_state::AttributeId::StateValue as _,
        ),
        Property::Battery => (
            sensors::POWER_SOURCE_CLUSTER.id,
            power_source::AttributeId::BatPercentRemaining as _,
        ),
        _ => return None,
    })
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

fn sensor_properties(capabilities: &[Capability], clusters: &[Cluster<'_>]) -> Vec<Property> {
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
