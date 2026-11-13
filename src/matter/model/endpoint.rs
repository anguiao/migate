use super::*;

// Basic interoperability intentionally omits the standard's Lighting/Scenes requirements.
const ON_OFF_PLUG: DeviceType = DeviceType {
    dtype: 0x010a,
    drev: 3,
};
const WINDOW_COVERING: DeviceType = DeviceType {
    dtype: 0x0202,
    drev: 6,
};
const CURTAIN_CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    <curtain::CurtainHandler as window_covering::ClusterAsyncHandler>::CLUSTER
);
const AIR_CONDITIONER_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    <OnOffHandler<true> as on_off::ClusterAsyncHandler>::CLUSTER,
    <air_conditioner::Thermostat as thermostat::ClusterAsyncHandler>::CLUSTER,
    <air_conditioner::AirConditionerFan<false> as fan_control::ClusterAsyncHandler>::CLUSTER
);
const HEATER_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    <heater::HeaterThermostat as thermostat::ClusterAsyncHandler>::CLUSTER
);
const AIR_CONDITIONER_ROCKING_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    <OnOffHandler<true> as on_off::ClusterAsyncHandler>::CLUSTER,
    <air_conditioner::Thermostat as thermostat::ClusterAsyncHandler>::CLUSTER,
    <air_conditioner::AirConditionerFan<true> as fan_control::ClusterAsyncHandler>::CLUSTER
);
const FAN_CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    OnOffHandler::<false>::CLUSTER,
    <fan::FanControl<false> as fan_control::ClusterAsyncHandler>::CLUSTER
);
const FAN_ROCKING_CHILD_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    groups::GroupsHandler::CLUSTER,
    identify::IdentifyHandler::<()>::CLUSTER,
    OnOffHandler::<false>::CLUSTER,
    <fan::FanControl<true> as fan_control::ClusterAsyncHandler>::CLUSTER
);
const CT_LIGHT: DeviceType = DeviceType {
    dtype: 0x010c,
    drev: 4,
};
const RGB_LIGHT: DeviceType = DeviceType {
    dtype: 0x010d,
    drev: 4,
};
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
const RELAY_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    BridgedHandler::CLUSTER,
    binding::CLUSTER,
    OnOffHandler::<false>::CLUSTER
);
const RELAY_LIGHT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    BridgedHandler::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    lighting::IDENTIFY,
    OnOffHandler::<false>::CLUSTER
);
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
    OnOffHandler::<false>::CLUSTER
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

const CONTACT_SENSOR: DeviceType = DeviceType {
    dtype: 0x0015,
    drev: 2,
};
const OCCUPANCY_SENSOR: DeviceType = DeviceType {
    dtype: 0x0107,
    drev: 4,
};
const CONTACT_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <states::ContactHandler as boolean_state::ClusterHandler>::CLUSTER
);
const OCCUPANCY_CLUSTERS: &[rs_matter::dm::Cluster<'static>] = clusters!(
    desc::CLUSTER_TAG_LIST,
    identify::IdentifyHandler::<()>::CLUSTER,
    <states::OccupancyHandler as occupancy_sensing::ClusterHandler>::CLUSTER
);

pub(super) struct Entry {
    pub(super) endpoint: u16,
    pub(super) descriptor: Dataver,
    pub(super) role: Role,
}
pub(super) enum Role {
    Relay {
        device_type: crate::configuration::RelayType,
        function: OnOffHandler,
        bridged: BridgedHandler,
        bindings: Rc<BindingRegistry>,
        binding: Dataver,
        groups: Dataver,
        identify: identify::IdentifyHandler,
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
impl Role {
    fn function(&self) -> Option<&FunctionHandler> {
        match self {
            Self::Child { function, .. } => Some(function),
            Self::Parent { .. } | Self::Relay { .. } => None,
        }
    }
    fn bridged(&self) -> Option<&BridgedHandler> {
        match self {
            Self::Parent { bridged, .. } | Self::Relay { bridged, .. } => Some(bridged),
            Self::Child { .. } => None,
        }
    }
    fn battery(&self) -> Option<&sensors::BatteryHandler> {
        match self {
            Self::Parent { battery, .. } => battery.as_ref(),
            Self::Child { .. } | Self::Relay { .. } => None,
        }
    }
}
// Each role composes only its supported clusters; rs-matter owns operation routing.
macro_rules! dispatch {
    ($entry:expr, $method:ident($($arg:expr),* $(,)?)) => {
        match &$entry.role {
            Role::Relay { function, bridged, bindings, binding, groups, identify, device_type } => {
                let binding_match: fn(u16, u32) -> bool = if *device_type == crate::configuration::RelayType::Switch {
                    |_, cluster| cluster == 0x1e
                } else { |_, _| false };
                let groups_match: fn(u16, u32) -> bool = if *device_type == crate::configuration::RelayType::Light {
                    |_, cluster| cluster == 4
                } else { |_, _| false };
                let identify_match: fn(u16, u32) -> bool = if *device_type == crate::configuration::RelayType::Light {
                    |_, cluster| cluster == 3
                } else { |_, _| false };
                AsyncHandler::$method(&EmptyHandler
                    .chain(|_, cluster| cluster == 0x39, Async(bridged::HandlerAdaptor(bridged)))
                    .chain(binding_match, Async(BindingHandler::new(Dataver::new(binding.get()), $entry.endpoint, bindings).adapt()))
                    .chain(groups_match, Async(groups::GroupsHandler::new_with_identify(Dataver::new(groups.get()), identify).adapt()))
                    .chain(identify_match, Async(identify::HandlerAdaptor(identify)))
                    .chain(|_, cluster| cluster == 6, on_off::HandlerAsyncAdaptor(function)), $($arg),*
                ).await
            },
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
    pub(super) fn metadata<'a>(&'a self, tags: &'a [SemanticTag<'a>]) -> Endpoint<'a> {
        match &self.role {
            Role::Relay {
                device_type: crate::configuration::RelayType::Switch,
                ..
            } => Endpoint::new_with_clients(
                self.endpoint,
                devices!(DEV_TYPE_BRIDGED_NODE, DEV_TYPE_ON_OFF_LIGHT_SWITCH),
                RELAY_CLUSTERS,
                &[on_off::FULL_CLUSTER.id],
            ),
            Role::Relay {
                device_type: crate::configuration::RelayType::Light,
                ..
            } => Endpoint::new(
                self.endpoint,
                devices!(DEV_TYPE_BRIDGED_NODE, DEV_TYPE_ON_OFF_LIGHT),
                RELAY_LIGHT_CLUSTERS,
            ),
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
                FunctionHandlerKind::AirConditioner(_) => Endpoint::new(
                    self.endpoint,
                    AIR_CONDITIONER_DEVICE_TYPES,
                    AIR_CONDITIONER_CLUSTERS,
                )
                .with_tags(tags),
                FunctionHandlerKind::AirConditionerSwing(_) => Endpoint::new(
                    self.endpoint,
                    AIR_CONDITIONER_DEVICE_TYPES,
                    AIR_CONDITIONER_ROCKING_CLUSTERS,
                )
                .with_tags(tags),
                FunctionHandlerKind::Heater(_) => {
                    Endpoint::new(self.endpoint, HEATER_DEVICE_TYPES, HEATER_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::Fan(_) => {
                    Endpoint::new(self.endpoint, devices!(FAN_DEVICE_TYPE), FAN_CHILD_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::FanSwing(_) => Endpoint::new(
                    self.endpoint,
                    devices!(FAN_DEVICE_TYPE),
                    FAN_ROCKING_CHILD_CLUSTERS,
                )
                .with_tags(tags),
                FunctionHandlerKind::Curtain(_) => Endpoint::new(
                    self.endpoint,
                    devices!(WINDOW_COVERING),
                    CURTAIN_CHILD_CLUSTERS,
                )
                .with_tags(tags),
                FunctionHandlerKind::Contact(_) => {
                    Endpoint::new(self.endpoint, devices!(CONTACT_SENSOR), CONTACT_CLUSTERS)
                        .with_tags(tags)
                }
                FunctionHandlerKind::Occupancy(_) => Endpoint::new(
                    self.endpoint,
                    devices!(OCCUPANCY_SENSOR),
                    OCCUPANCY_CLUSTERS,
                )
                .with_tags(tags),
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
    pub(super) async fn read(
        &self,
        ctx: impl ReadContext,
        reply: impl ReadReply,
    ) -> Result<(), Error> {
        if ctx.attr().cluster_id == 0x1d {
            Handler::read(&self.descriptor().adapt(), ctx, reply)
        } else {
            dispatch!(self, read(ctx, reply))
        }
    }
    pub(super) async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        dispatch!(self, write(ctx))
    }
    pub(super) async fn invoke(
        &self,
        ctx: impl InvokeContext,
        reply: impl InvokeReply,
    ) -> Result<(), Error> {
        dispatch!(self, invoke(ctx, reply))
    }
    pub(super) fn bump(&self, cluster: Option<u32>) {
        if cluster.is_none_or(|id| id == 0x1d) {
            self.descriptor.changed();
        }
        if let Some(function) = self.role.function() {
            function.bump(cluster);
        }
        if let Role::Relay {
            function,
            binding,
            groups,
            identify,
            ..
        } = &self.role
        {
            if cluster.is_none_or(|id| id == 6) {
                function.dataver_changed();
            }
            if cluster.is_none_or(|id| id == 0x1e) {
                binding.changed();
            }
            if cluster.is_none_or(|id| id == 4) {
                groups.changed();
            }
            if cluster.is_none_or(|id| id == 3) {
                identify.dataver_changed();
            }
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
    pub(super) async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
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
impl desc::PartsMatcher for Entry {
    fn matches(&self, _our_endpoint: u16, endpoint: u16) -> bool {
        match &self.role {
            Role::Parent { children, .. } => children.contains(&endpoint),
            Role::Child { .. } | Role::Relay { .. } => false,
        }
    }
}
