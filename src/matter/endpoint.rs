use super::{
    common, curtain, fan, lighting,
    reporting::ReportedState,
    rvc, sensors,
    storage::StoreAdapter,
    thermostat as thermostat_handler,
    topology::{EndpointPlan, exposed_properties},
};
use crate::{
    device::{Capability, DeviceService},
    storage::{FeatureIdentity as AllocatedFeature, StorageError},
};
use rand::RngExt as _;
use rs_matter::dm::{
    Cluster, Dataver, DeviceType,
    clusters::{desc, groups, identify, scenes},
};
use std::cell::RefCell;

pub(super) struct FeatureRuntime {
    pub(super) allocation: AllocatedFeature,
    pub(super) device_types: Vec<DeviceType>,
    pub(super) clusters: Vec<Cluster<'static>>,
    pub(super) shape_signature: String,
    pub(super) config_signature: RefCell<String>,
    pub(super) reported: ReportedState,
    pub(super) desc: desc::DescHandler<'static>,
    pub(super) identify: identify::IdentifyHandler,
    pub(super) common: common::CommonHandler,
    pub(super) sensor: sensors::SensorHandler,
    pub(super) lighting: Option<lighting::LightingHandler>,
    pub(super) fan: Option<fan::FanHandler>,
    pub(super) thermostat: Option<thermostat_handler::ThermostatHandler>,
    pub(super) curtain: Option<curtain::CurtainHandler>,
    pub(super) rvc: Option<rvc::RvcHandler>,
    pub(super) groups: groups::GroupsHandler<'static>,
    pub(super) scenes: scenes::ScenesState<33, 96>,
    pub(super) scenes_dataver: Dataver,
}

impl FeatureRuntime {
    pub(super) fn lighting(&self) -> &lighting::LightingHandler {
        self.lighting
            .as_ref()
            .expect("lighting clusters require a lighting handler")
    }

    pub(super) fn fan(&self) -> &fan::FanHandler {
        self.fan
            .as_ref()
            .expect("fan clusters require a fan handler")
    }

    pub(super) fn thermostat(&self) -> &thermostat_handler::ThermostatHandler {
        self.thermostat
            .as_ref()
            .expect("thermostat clusters require a thermostat handler")
    }

    pub(super) fn curtain(&self) -> &curtain::CurtainHandler {
        self.curtain
            .as_ref()
            .expect("window covering cluster requires a curtain handler")
    }

    pub(super) fn rvc(&self) -> &rvc::RvcHandler {
        self.rvc
            .as_ref()
            .expect("RVC clusters require an RVC handler")
    }
}

pub(super) fn build_runtime(
    service: &DeviceService,
    store: &StoreAdapter,
    allocation: AllocatedFeature,
    name: String,
    capabilities: Vec<Capability>,
    plan: EndpointPlan,
) -> Result<FeatureRuntime, StorageError> {
    let EndpointPlan {
        device_types,
        clusters,
        shape_signature,
        config_signature,
        has_power,
        has_fan,
        has_thermostat,
        has_curtain,
        has_rvc,
    } = plan;
    let label_override = store.capture(store.storage().feature_label(allocation.endpoint))?;
    let seed = rand::rng().random::<u32>();
    let reachable = service.is_available(&allocation.feature);
    let exposed_values = exposed_properties(&capabilities, &clusters)
        .into_iter()
        .map(|property| {
            let value = service
                .snapshot(&allocation.feature)
                .and_then(|snapshot| snapshot.property(property).cloned())
                .and_then(|state| match state {
                    crate::device::PropertyState::Current { value, .. } => Some(value),
                    _ => None,
                });
            (property, value)
        })
        .collect();
    Ok(FeatureRuntime {
        desc: desc::DescHandler::new(Dataver::new(seed)),
        identify: identify::IdentifyHandler::new(Dataver::new(seed.wrapping_add(1))),
        common: common::CommonHandler::new(
            Dataver::new(seed.wrapping_add(2)),
            service.clone(),
            allocation.feature.clone(),
            allocation.endpoint,
            allocation.public_id.as_str().to_owned(),
            name,
            label_override,
            store.clone(),
        ),
        sensor: sensors::SensorHandler::new(
            service.clone(),
            allocation.feature.clone(),
            capabilities.clone(),
            allocation.endpoint,
            seed.wrapping_add(3),
        ),
        lighting: has_power.then(|| {
            lighting::LightingHandler::new(
                service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                allocation.endpoint,
                seed.wrapping_add(9),
            )
        }),
        fan: has_fan.then(|| {
            fan::FanHandler::new(
                service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                seed.wrapping_add(10),
            )
        }),
        thermostat: has_thermostat.then(|| {
            thermostat_handler::ThermostatHandler::new(
                service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                seed.wrapping_add(11),
            )
        }),
        curtain: has_curtain.then(|| {
            curtain::CurtainHandler::new(
                service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                seed.wrapping_add(14),
            )
        }),
        rvc: has_rvc.then(|| {
            rvc::RvcHandler::new(
                service.clone(),
                allocation.feature.clone(),
                capabilities.clone(),
                allocation.endpoint,
                seed.wrapping_add(15),
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
        reported: ReportedState::new(exposed_values, reachable),
    })
}
