use super::{
    common,
    endpoint::FeatureRuntime,
    lighting, reporting, rvc,
    scene_context::{SceneContext, SceneValidReadReply},
    sensors,
    storage::StoreAdapter,
    topology::TopologyRegistry,
};
use crate::{
    device::{DeviceChange, DeviceService, FeatureIdentity},
    storage::{DeviceStore, MatterStore, StorageError},
};
use futures_lite::future;
use rand::RngExt as _;
use rs_matter::dm::clusters::{desc::ClusterHandler as _, identify::ClusterHandler as _};
use rs_matter::{
    dm::{
        AsyncHandler, Dataver, Endpoint, Handler, HandlerContext, InvokeContext, InvokeReply,
        MatchContext, Metadata, Node, ReadContext, ReadReply, WriteContext,
        clusters::decl::{
            boolean_state, bridged_device_basic_information as bridged, fan_control,
            illuminance_measurement, occupancy_sensing, power_source,
            relative_humidity_measurement, rvc_clean_mode, rvc_operational_state, rvc_run_mode,
            temperature_measurement, thermostat, window_covering,
        },
        clusters::{desc, groups, identify, scenes},
    },
    error::{Error, ErrorCode},
};
use std::rc::Rc;

/// Dynamic Matter dispatch backed by the authoritative device service.
pub struct DeviceBridgeModel {
    service: DeviceService,
    store: StoreAdapter,
    topology: TopologyRegistry,
    aggregator_desc: desc::DescHandler<'static>,
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

    pub(super) fn with_store(
        service: DeviceService,
        devices: DeviceStore,
        store: StoreAdapter,
    ) -> Result<Self, StorageError> {
        let topology = TopologyRegistry::new(service.clone(), devices, store.clone())?;
        Ok(Self {
            service,
            store,
            topology,
            aggregator_desc: desc::DescHandler::new_aggregator(Dataver::new(rand::rng().random())),
        })
    }

    pub fn endpoint_for(&self, feature: &FeatureIdentity) -> Option<u16> {
        self.topology.endpoint_for(feature)
    }
    fn snapshot(&self) -> Rc<Vec<Rc<FeatureRuntime>>> {
        self.topology.snapshot()
    }
    pub fn check_failure(&self) -> Result<(), StorageError> {
        self.store.check_failure()
    }
    pub fn take_rebuild_request(&self) -> bool {
        self.topology.take_rebuild_request()
    }
    pub async fn rebuild_requested(&self) {
        self.topology.rebuild_requested().await
    }
    pub fn topology_signature(&self) -> String {
        self.topology.topology_signature()
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
            id if id == fan_control::FULL_CLUSTER.id => {
                AsyncHandler::read(&fan_control::HandlerAsyncAdaptor(runtime.fan()), ctx, reply)
                    .await
            }
            id if id == thermostat::FULL_CLUSTER.id => {
                AsyncHandler::read(
                    &thermostat::HandlerAsyncAdaptor(runtime.thermostat()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == window_covering::FULL_CLUSTER.id => {
                AsyncHandler::read(
                    &window_covering::HandlerAsyncAdaptor(runtime.curtain()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::RUN_CLUSTER.id => {
                AsyncHandler::read(
                    &rvc_run_mode::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::CLEAN_CLUSTER.id => {
                AsyncHandler::read(
                    &rvc_clean_mode::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::OPERATIONAL_CLUSTER.id => {
                AsyncHandler::read(
                    &rvc_operational_state::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
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

    async fn dispatch_write(&self, ctx: impl WriteContext) -> Result<(), Error> {
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
            id if id == fan_control::FULL_CLUSTER.id => {
                AsyncHandler::write(&fan_control::HandlerAsyncAdaptor(runtime.fan()), ctx).await
            }
            id if id == thermostat::FULL_CLUSTER.id => {
                AsyncHandler::write(&thermostat::HandlerAsyncAdaptor(runtime.thermostat()), ctx)
                    .await
            }
            id if id == window_covering::FULL_CLUSTER.id => {
                AsyncHandler::write(
                    &window_covering::HandlerAsyncAdaptor(runtime.curtain()),
                    ctx,
                )
                .await
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
            id if id == thermostat::FULL_CLUSTER.id => {
                AsyncHandler::invoke(
                    &thermostat::HandlerAsyncAdaptor(runtime.thermostat()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == window_covering::FULL_CLUSTER.id => {
                AsyncHandler::invoke(
                    &window_covering::HandlerAsyncAdaptor(runtime.curtain()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::RUN_CLUSTER.id => {
                AsyncHandler::invoke(
                    &rvc_run_mode::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::CLEAN_CLUSTER.id => {
                AsyncHandler::invoke(
                    &rvc_clean_mode::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
            }
            id if id == rvc::OPERATIONAL_CLUSTER.id => {
                AsyncHandler::invoke(
                    &rvc_operational_state::HandlerAsyncAdaptor(runtime.rvc()),
                    ctx,
                    reply,
                )
                .await
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
}

impl Metadata for DeviceBridgeModel {
    fn access<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Node<'_>) -> R,
    {
        let runtimes = self.snapshot();
        let mut endpoints = Vec::with_capacity(runtimes.len() + 2);
        endpoints.push(common::BASE_NODE.endpoints[0].clone());
        endpoints.push(common::BASE_NODE.endpoints[1].clone());
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
        true
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
        self.dispatch_write(ctx)
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
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == fan_control::FULL_CLUSTER.id)
                && let Some(fan) = &runtime.fan
            {
                fan.dataver().changed();
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == thermostat::FULL_CLUSTER.id)
                && let Some(thermostat) = &runtime.thermostat
            {
                thermostat.dataver().changed();
            }
            if ctx
                .cluster()
                .is_none_or(|cluster| cluster == window_covering::FULL_CLUSTER.id)
                && let Some(curtain) = &runtime.curtain
            {
                curtain.dataver().changed();
            }
            if let Some(handler) = &runtime.rvc {
                if let Some(cluster) = ctx.cluster() {
                    if let Some(dataver) = handler.dataver(cluster) {
                        dataver.changed();
                    }
                } else {
                    for cluster in [
                        rvc::RUN_CLUSTER.id,
                        rvc::CLEAN_CLUSTER.id,
                        rvc::OPERATIONAL_CLUSTER.id,
                    ] {
                        if let Some(dataver) = handler.dataver(cluster) {
                            dataver.changed();
                        }
                    }
                }
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
        self.topology.reconcile(&ctx)?;
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
                reporting::process_state_change(&self.service, &self.snapshot(), &ctx, change)?;
            }
            if reconcile {
                self.topology.reconcile(&ctx)?;
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
