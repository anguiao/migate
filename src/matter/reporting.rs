use super::{
    common, endpoint::FeatureRuntime, lighting, rvc, sensors, topology::exposed_properties,
};
use crate::device::{Capability, DeviceChange, DeviceService, Property, PropertyValue};
use rs_matter::{
    dm::{
        HandlerContext,
        clusters::decl::{
            boolean_state, bridged_device_basic_information as bridged, fan_control,
            illuminance_measurement, occupancy_sensing, power_source,
            relative_humidity_measurement, rvc_clean_mode, rvc_operational_state, rvc_run_mode,
            temperature_measurement, thermostat, window_covering,
        },
    },
    error::Error,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

pub(super) struct ReportedState {
    exposed_values: RefCell<BTreeMap<Property, Option<PropertyValue>>>,
    reachable: Cell<bool>,
}

impl ReportedState {
    pub(super) fn new(
        exposed_values: BTreeMap<Property, Option<PropertyValue>>,
        reachable: bool,
    ) -> Self {
        Self {
            exposed_values: RefCell::new(exposed_values),
            reachable: Cell::new(reachable),
        }
    }
}

pub(super) fn process_state_change(
    service: &DeviceService,
    runtimes: &[Rc<FeatureRuntime>],
    ctx: &impl HandlerContext,
    change: DeviceChange,
) -> Result<(), Error> {
    match change {
        DeviceChange::AvailabilityChanged { feature, available } => {
            if let Some(runtime) = runtimes
                .iter()
                .find(|runtime| runtime.allocation.feature == feature)
            {
                report_reachability(ctx, runtime, available)?;
            }
        }
        DeviceChange::StateChanged {
            feature,
            properties,
        } => {
            let Some(runtime) = runtimes
                .iter()
                .find(|runtime| runtime.allocation.feature == feature)
            else {
                return Ok(());
            };
            let endpoint = runtime.allocation.endpoint;
            for property in properties {
                if property == Property::VacuumFault
                    && let Some(rvc) = &runtime.rvc
                {
                    rvc.observe_fault(ctx)?;
                }
                if !runtime
                    .reported
                    .exposed_values
                    .borrow()
                    .contains_key(&property)
                {
                    continue;
                }
                let current = service
                    .snapshot(&feature)
                    .and_then(|snapshot| snapshot.property(property).cloned())
                    .and_then(|state| match state {
                        crate::device::PropertyState::Current { value, .. } => Some(value),
                        _ => None,
                    });
                let mut exposed = runtime.reported.exposed_values.borrow_mut();
                if exposed.get(&property) == Some(&current) {
                    continue;
                }
                let previous = exposed.insert(property, current.clone()).flatten();
                if runtime.lighting.is_some()
                    && matches!(
                        property,
                        Property::Power
                            | Property::Brightness
                            | Property::ColorTemperature
                            | Property::Color
                    )
                {
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
                        .lighting
                        .as_ref()
                        .expect("brightness requires a lighting handler")
                        .should_report_brightness(previous.as_ref(), current.as_ref());
                if report_property {
                    for (cluster, attribute) in property_paths(runtime, property) {
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

pub(super) fn report_reachability(
    ctx: &impl HandlerContext,
    runtime: &FeatureRuntime,
    available: bool,
) -> Result<(), Error> {
    if runtime.reported.reachable.replace(available) == available {
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

pub(super) fn reconcile_values(
    ctx: &impl HandlerContext,
    service: &DeviceService,
    runtime: &FeatureRuntime,
    capabilities: &[Capability],
) -> Result<(), Error> {
    for property in exposed_properties(capabilities, &runtime.clusters) {
        let current = service
            .snapshot(&runtime.allocation.feature)
            .and_then(|snapshot| snapshot.property(property).cloned())
            .and_then(|state| match state {
                crate::device::PropertyState::Current { value, .. } => Some(value),
                _ => None,
            });
        if property == Property::VacuumFault
            && let Some(rvc) = &runtime.rvc
        {
            rvc.observe_fault(ctx)?;
        }
        let mut exposed = runtime.reported.exposed_values.borrow_mut();
        if exposed.get(&property) != Some(&current) {
            exposed.insert(property, current);
            for (cluster, attribute) in property_paths(runtime, property) {
                ctx.notify_attr_changed(runtime.allocation.endpoint, cluster, attribute);
            }
        }
    }
    Ok(())
}

pub(super) fn notify_configuration(ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
    notify_sensor_configuration(ctx, runtime);
    notify_fan_configuration(ctx, runtime);
    notify_thermostat_configuration(ctx, runtime);
    notify_rvc_configuration(ctx, runtime);
}

fn notify_sensor_configuration(ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
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

fn notify_fan_configuration(ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
    if runtime.fan.is_none() {
        return;
    }
    for attribute in [
        fan_control::AttributeId::FanMode,
        fan_control::AttributeId::FanModeSequence,
        fan_control::AttributeId::PercentSetting,
        fan_control::AttributeId::PercentCurrent,
        fan_control::AttributeId::SpeedMax,
        fan_control::AttributeId::SpeedSetting,
        fan_control::AttributeId::SpeedCurrent,
        fan_control::AttributeId::RockSupport,
        fan_control::AttributeId::RockSetting,
    ] {
        if runtime
            .clusters
            .iter()
            .find(|cluster| cluster.id == fan_control::FULL_CLUSTER.id)
            .and_then(|cluster| cluster.attribute(attribute as _))
            .is_some()
        {
            ctx.notify_attr_changed(
                runtime.allocation.endpoint,
                fan_control::FULL_CLUSTER.id,
                attribute as _,
            );
        }
    }
}

fn notify_thermostat_configuration(ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
    if runtime.thermostat.is_none() {
        return;
    }
    for attribute in [
        thermostat::AttributeId::AbsMinHeatSetpointLimit,
        thermostat::AttributeId::AbsMaxHeatSetpointLimit,
        thermostat::AttributeId::AbsMinCoolSetpointLimit,
        thermostat::AttributeId::AbsMaxCoolSetpointLimit,
        thermostat::AttributeId::OccupiedHeatingSetpoint,
        thermostat::AttributeId::OccupiedCoolingSetpoint,
        thermostat::AttributeId::ControlSequenceOfOperation,
        thermostat::AttributeId::SystemMode,
    ] {
        if runtime
            .clusters
            .iter()
            .find(|cluster| cluster.id == thermostat::FULL_CLUSTER.id)
            .and_then(|cluster| cluster.attribute(attribute as _))
            .is_some()
        {
            ctx.notify_attr_changed(
                runtime.allocation.endpoint,
                thermostat::FULL_CLUSTER.id,
                attribute as _,
            );
        }
    }
}

fn notify_rvc_configuration(ctx: &impl HandlerContext, runtime: &FeatureRuntime) {
    if runtime.rvc.is_none() {
        return;
    }
    for (cluster, attribute) in [
        (
            rvc::CLEAN_CLUSTER.id,
            rvc_clean_mode::AttributeId::SupportedModes as u32,
        ),
        (
            rvc::CLEAN_CLUSTER.id,
            rvc_clean_mode::AttributeId::CurrentMode as u32,
        ),
    ] {
        if runtime
            .clusters
            .iter()
            .find(|candidate| candidate.id == cluster)
            .and_then(|candidate| candidate.attribute(attribute))
            .is_some()
        {
            ctx.notify_attr_changed(runtime.allocation.endpoint, cluster, attribute);
        }
    }
}

fn property_paths(runtime: &FeatureRuntime, property: Property) -> Vec<(u32, u32)> {
    let single = |path| vec![path];
    let paths = match property {
        Property::Power => {
            let mut paths = Vec::new();
            if runtime.fan.is_some() {
                paths.extend(fan_state_paths());
            }
            if runtime.thermostat.is_some() {
                paths.extend(thermostat_state_paths());
            }
            if runtime.lighting.is_some() {
                paths.push((
                    lighting::ON_OFF_CLUSTER.id,
                    rs_matter::dm::clusters::decl::on_off::AttributeId::OnOff as _,
                ));
            }
            paths
        }
        Property::FanSpeed if runtime.fan.is_some() => fan_state_paths(),
        Property::Oscillation | Property::SwingMode if runtime.fan.is_some() => single((
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::RockSetting as _,
        )),
        Property::CurrentTemperature => single((
            thermostat::FULL_CLUSTER.id,
            thermostat::AttributeId::LocalTemperature as _,
        )),
        Property::TargetTemperature => vec![
            (
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedHeatingSetpoint as _,
            ),
            (
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
            ),
        ],
        Property::HvacMode => thermostat_state_paths(),
        Property::CurtainPosition => single((
            window_covering::FULL_CLUSTER.id,
            window_covering::AttributeId::CurrentPositionLiftPercent100ths as _,
        )),
        Property::CurtainTargetPosition => single((
            window_covering::FULL_CLUSTER.id,
            window_covering::AttributeId::TargetPositionLiftPercent100ths as _,
        )),
        Property::CurtainMovement => single((
            window_covering::FULL_CLUSTER.id,
            window_covering::AttributeId::OperationalStatus as _,
        )),
        Property::VacuumOperationalState => vec![
            (
                rvc::RUN_CLUSTER.id,
                rvc_run_mode::AttributeId::CurrentMode as _,
            ),
            (
                rvc::OPERATIONAL_CLUSTER.id,
                rvc_operational_state::AttributeId::OperationalState as _,
            ),
        ],
        Property::VacuumCleanMode => single((
            rvc::CLEAN_CLUSTER.id,
            rvc_clean_mode::AttributeId::CurrentMode as _,
        )),
        Property::VacuumFault => vec![
            (
                rvc::OPERATIONAL_CLUSTER.id,
                rvc_operational_state::AttributeId::OperationalState as _,
            ),
            (
                rvc::OPERATIONAL_CLUSTER.id,
                rvc_operational_state::AttributeId::OperationalError as _,
            ),
        ],
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
    };
    paths
        .into_iter()
        .filter(|(cluster, attribute)| {
            runtime
                .clusters
                .iter()
                .find(|candidate| candidate.id == *cluster)
                .and_then(|candidate| candidate.attribute(*attribute))
                .is_some()
        })
        .collect()
}

fn fan_state_paths() -> Vec<(u32, u32)> {
    [
        fan_control::AttributeId::FanMode,
        fan_control::AttributeId::PercentSetting,
        fan_control::AttributeId::PercentCurrent,
        fan_control::AttributeId::SpeedSetting,
        fan_control::AttributeId::SpeedCurrent,
    ]
    .into_iter()
    .map(|attribute| (fan_control::FULL_CLUSTER.id, attribute as _))
    .collect()
}

fn thermostat_state_paths() -> Vec<(u32, u32)> {
    [
        thermostat::AttributeId::SystemMode,
        thermostat::AttributeId::OccupiedHeatingSetpoint,
        thermostat::AttributeId::OccupiedCoolingSetpoint,
    ]
    .into_iter()
    .map(|attribute| (thermostat::FULL_CLUSTER.id, attribute as _))
    .collect()
}
