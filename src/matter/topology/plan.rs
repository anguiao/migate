use super::super::{
    common, curtain, fan, lighting, rvc, sensors, thermostat as thermostat_handler,
};
use crate::device::{Capability, FeatureRole, Property};
use rs_matter::dm::clusters::identify::ClusterHandler as _;
use rs_matter::dm::{
    Cluster, DeviceType,
    clusters::decl::{fan_control, occupancy_sensing, thermostat, window_covering},
    clusters::{desc, identify},
    devices::{DEV_TYPE_BRIDGED_NODE, DEV_TYPE_POWER_SOURCE},
};
use sha1::{Digest, Sha1};

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
const DEV_TYPE_FAN: DeviceType = DeviceType {
    dtype: 0x002b,
    drev: 3,
};
const DEV_TYPE_THERMOSTAT: DeviceType = DeviceType {
    dtype: 0x0301,
    drev: 4,
};
const DEV_TYPE_WINDOW_COVERING: DeviceType = DeviceType {
    dtype: 0x0202,
    drev: 3,
};
const DEV_TYPE_ROBOTIC_VACUUM_CLEANER: DeviceType = DeviceType {
    dtype: 0x0074,
    drev: 3,
};

pub(in crate::matter) struct EndpointPlan {
    pub(in crate::matter) device_types: Vec<DeviceType>,
    pub(in crate::matter) clusters: Vec<Cluster<'static>>,
    pub(in crate::matter) shape_signature: String,
    pub(in crate::matter) config_signature: String,
    pub(in crate::matter) has_power: bool,
    pub(in crate::matter) has_fan: bool,
    pub(in crate::matter) has_thermostat: bool,
    pub(in crate::matter) has_curtain: bool,
    pub(in crate::matter) has_rvc: bool,
}

/// Derive protocol metadata without allocating identities or creating handlers.
pub(in crate::matter) fn plan_endpoint(
    role: FeatureRole,
    capabilities: &[Capability],
) -> Option<EndpointPlan> {
    let mut device_types = Vec::new();
    let mut clusters = vec![desc::CLUSTER_ENDPOINT_UNIQUE_ID, common::BRIDGED_CLUSTER];
    let has_temperature = role == FeatureRole::TemperatureSensor
        && capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Temperature(_)));
    let has_humidity = role == FeatureRole::HumiditySensor
        && capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Humidity(_)));
    let has_lux = role == FeatureRole::IlluminanceSensor
        && capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Illuminance(_)));
    let has_occupancy = matches!(
        role,
        FeatureRole::MotionSensor | FeatureRole::OccupancySensor
    ) && capabilities
        .iter()
        .any(|cap| matches!(cap, Capability::Motion | Capability::Occupancy));
    let has_contact =
        role == FeatureRole::ContactSensor && capabilities.contains(&Capability::Contact);
    let has_battery = capabilities
        .iter()
        .any(|cap| matches!(cap, Capability::Battery(_)));
    let has_power = matches!(
        role,
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
    let has_fan = matches!(
        role,
        FeatureRole::Fan | FeatureRole::BathHeaterSupplyFan | FeatureRole::BathHeaterExhaustFan
    ) && capabilities
        .iter()
        .any(|capability| matches!(capability, Capability::Power { writable: true }));
    let (fan_multi_speed, fan_auto, fan_rocking) = fan::capability_shape(capabilities);
    let thermostat_role = matches!(role, FeatureRole::Climate | FeatureRole::BathHeaterClimate);
    let (thermostat_heating, thermostat_cooling, thermostat_local_temperature) =
        thermostat_handler::capability_shape(capabilities, role);
    let has_thermostat = thermostat_role
        && (thermostat_heating || thermostat_cooling)
        && capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::Power { writable: true }))
        && capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::TargetTemperature(_)));
    let has_climate_fan = role == FeatureRole::Climate && (fan_multi_speed || fan_rocking);
    let has_curtain = role == FeatureRole::Curtain
        && capabilities
            .iter()
            .any(|capability| {
                matches!(capability, Capability::CurtainPosition(range) if range.accepts(0.0) && range.accepts(100.0))
            })
        && capabilities.contains(&Capability::CurtainStop);
    let has_rvc = role == FeatureRole::Vacuum && capabilities.contains(&Capability::VacuumControl);
    let has_rvc_clean = has_rvc
        && capabilities.iter().any(|capability| {
            matches!(capability, Capability::VacuumCleanModes(values) if !values.is_empty())
        });
    let has_rvc_dock = has_rvc && capabilities.contains(&Capability::VacuumDock);
    if !(has_temperature
        || has_humidity
        || has_lux
        || has_occupancy
        || has_contact
        || has_power
        || has_fan
        || has_thermostat
        || has_curtain
        || has_rvc)
    {
        return None;
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
        if role == FeatureRole::Load {
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
    if has_fan {
        device_types.push(DEV_TYPE_FAN);
        clusters.extend([
            lighting::GROUPS_CLUSTER,
            fan::cluster(fan_multi_speed, fan_auto, fan_rocking),
        ]);
    }
    if has_thermostat {
        device_types.push(DEV_TYPE_THERMOSTAT);
        clusters.extend([
            lighting::GROUPS_CLUSTER,
            thermostat_handler::cluster(
                thermostat_heating,
                thermostat_cooling,
                thermostat_local_temperature,
            ),
        ]);
        if has_climate_fan {
            clusters.push(fan::cluster(fan_multi_speed, fan_auto, fan_rocking));
        }
    }
    if has_curtain {
        device_types.push(DEV_TYPE_WINDOW_COVERING);
        clusters.extend([lighting::GROUPS_CLUSTER, curtain::CLUSTER]);
    }
    if has_rvc {
        device_types.push(DEV_TYPE_ROBOTIC_VACUUM_CLEANER);
        clusters.push(rvc::RUN_CLUSTER);
        if has_rvc_clean {
            clusters.push(rvc::CLEAN_CLUSTER);
        }
        clusters.push(rvc::operational_cluster(has_rvc_dock));
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
    let config_signature = config_signature(&shape_signature, capabilities);
    Some(EndpointPlan {
        device_types,
        clusters,
        shape_signature,
        config_signature,
        has_power,
        has_fan: has_fan || has_climate_fan,
        has_thermostat,
        has_curtain,
        has_rvc,
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

pub(in crate::matter) fn config_signature(shape: &str, capabilities: &[Capability]) -> String {
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
            Capability::FanSpeeds(values) => {
                digest.update(b"fan-speeds");
                digest.update((values.len() as u32).to_be_bytes());
                for value in values {
                    digest.update(value.to_be_bytes());
                }
            }
            Capability::SwingModes(values) => {
                digest.update(b"swing-modes");
                digest.update((values.len() as u32).to_be_bytes());
                for value in values {
                    digest.update([*value as u8]);
                }
            }
            Capability::TargetTemperature(range) => {
                digest.update(b"target-temperature");
                digest.update(range.minimum.to_bits().to_be_bytes());
                digest.update(range.maximum.to_bits().to_be_bytes());
                digest.update(range.step.to_bits().to_be_bytes());
            }
            Capability::CurtainPosition(range) => {
                digest.update(b"curtain-position");
                digest.update(range.minimum.to_bits().to_be_bytes());
                digest.update(range.maximum.to_bits().to_be_bytes());
                digest.update(range.step.to_bits().to_be_bytes());
            }
            Capability::HvacModes(values) => {
                digest.update(b"hvac-modes");
                for value in values {
                    digest.update([*value as u8]);
                }
            }
            Capability::VacuumCleanModes(values) => {
                digest.update(b"vacuum-clean-modes");
                digest.update((values.len() as u32).to_be_bytes());
                for value in values {
                    digest.update([*value as u8]);
                }
            }
            _ => {}
        }
    }
    hex_digest(digest.finalize().as_slice())
}

pub(in crate::matter) fn exposed_properties(
    capabilities: &[Capability],
    clusters: &[Cluster<'_>],
) -> Vec<Property> {
    let has = |cluster| clusters.iter().any(|item| item.id == cluster);
    let mut properties = Vec::new();
    let mut push = |property| {
        if !properties.contains(&property) {
            properties.push(property);
        }
    };
    for capability in capabilities {
        match capability {
            Capability::Temperature(_) if has(sensors::TEMPERATURE_CLUSTER.id) => {
                push(Property::Temperature)
            }
            Capability::Temperature(_) if has(thermostat::FULL_CLUSTER.id) => {
                push(Property::CurrentTemperature)
            }
            Capability::Humidity(_) if has(sensors::HUMIDITY_CLUSTER.id) => {
                push(Property::Humidity)
            }
            Capability::Illuminance(_) if has(sensors::ILLUMINANCE_CLUSTER.id) => {
                push(Property::Illuminance)
            }
            Capability::Motion if has(occupancy_sensing::FULL_CLUSTER.id) => push(Property::Motion),
            Capability::Occupancy if has(occupancy_sensing::FULL_CLUSTER.id) => {
                push(Property::Occupancy)
            }
            Capability::Contact if has(sensors::BOOLEAN_STATE_CLUSTER.id) => {
                push(Property::Contact)
            }
            Capability::Battery(_) if has(sensors::POWER_SOURCE_CLUSTER.id) => {
                push(Property::Battery)
            }
            Capability::Power { .. }
                if has(fan_control::FULL_CLUSTER.id)
                    || has(thermostat::FULL_CLUSTER.id)
                    || has(lighting::ON_OFF_CLUSTER.id) =>
            {
                push(Property::Power)
            }
            Capability::TargetTemperature(_) if has(thermostat::FULL_CLUSTER.id) => {
                push(Property::TargetTemperature)
            }
            Capability::HvacModes(_) if has(thermostat::FULL_CLUSTER.id) => {
                push(Property::HvacMode)
            }
            Capability::FanSpeeds(_) if has(fan_control::FULL_CLUSTER.id) => {
                push(Property::FanSpeed)
            }
            Capability::SwingModes(_) if has(fan_control::FULL_CLUSTER.id) => {
                push(if has(thermostat::FULL_CLUSTER.id) {
                    Property::SwingMode
                } else {
                    Property::Oscillation
                })
            }
            Capability::Brightness(_) if has(lighting::LEVEL_CLUSTER.id) => {
                push(Property::Brightness)
            }
            Capability::ColorTemperature(_)
                if has(rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id) =>
            {
                push(Property::ColorTemperature)
            }
            Capability::Color
                if has(rs_matter::dm::clusters::decl::color_control::FULL_CLUSTER.id) =>
            {
                push(Property::Color)
            }
            Capability::CurtainPosition(_) if has(window_covering::FULL_CLUSTER.id) => {
                push(Property::CurtainPosition);
                push(Property::CurtainTargetPosition);
                push(Property::CurtainMovement);
            }
            Capability::VacuumControl if has(rvc::RUN_CLUSTER.id) => {
                push(Property::VacuumOperationalState);
                push(Property::VacuumFault);
            }
            Capability::VacuumCleanModes(_) if has(rvc::CLEAN_CLUSTER.id) => {
                push(Property::VacuumCleanMode);
            }
            _ => {}
        }
    }
    properties
}

pub(in crate::matter) fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
