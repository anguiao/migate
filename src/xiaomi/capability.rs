use std::{error::Error, fmt};

use serde_json::Value;

use crate::device::{Capability, Category, DeviceDescription, DeviceKey};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedOutlet {
    pub description: DeviceDescription,
    pub main_power: PropertyBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet main power must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "outlet main power lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
        }
    }
}

impl Error for CapabilityError {}

/// Parse the standard outlet switch, excluding indicator and vendor services.
/// Instance IDs, vendor suffixes, descriptions, and order carry no semantics.
pub fn parse_outlet(key: DeviceKey, instance: &Value) -> Result<ParsedOutlet, CapabilityError> {
    let device_type = string(instance, "type")?;
    if !is_outlet_type(device_type) {
        return Err(CapabilityError::UnsupportedDevice);
    }
    let mut candidates = Vec::new();
    for service in array(instance, "services")? {
        let service_type = string(service, "type")?;
        if !is_standard_type(service_type, "service", "switch", "0000780C") {
            continue;
        }
        let siid = instance_id(service)?;
        for property in array(service, "properties")? {
            let property_type = string(property, "type")?;
            if is_standard_type(property_type, "property", "on", "00000006") {
                candidates.push((siid, property));
            }
        }
    }
    let (siid, property) = match candidates.as_slice() {
        [] => return Err(CapabilityError::MissingMainPower),
        [candidate] => *candidate,
        _ => return Err(CapabilityError::AmbiguousMainPower),
    };
    let piid = instance_id(property)?;
    if string(property, "format")? != "bool" {
        return Err(CapabilityError::InvalidMainPowerType);
    }
    let access = array(property, "access")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or(CapabilityError::InvalidDescription("access"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for permission in ["read", "write", "notify"] {
        if !access.contains(&permission) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(ParsedOutlet {
        description: DeviceDescription {
            key,
            category: Category::Outlet,
            capabilities: vec![Capability::OnOff],
        },
        main_power: PropertyBinding { siid, piid },
    })
}

pub(crate) fn is_outlet_type(value: &str) -> bool {
    is_standard_type(value, "device", "outlet", "0000A002")
}

fn is_standard_type(value: &str, entity: &str, name: &str, type_id: &str) -> bool {
    let parts = value.split(':').collect::<Vec<_>>();
    matches!(parts.as_slice(), ["urn", "miot-spec-v2", actual_entity, actual_name, actual_id, vendor, version]
        if *actual_entity == entity && *actual_name == name && *actual_id == type_id
            && !vendor.is_empty() && version.parse::<u32>().is_ok_and(|value| value > 0))
}

fn string<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, CapabilityError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CapabilityError::InvalidDescription(field))
}

fn array<'a>(value: &'a Value, field: &'static str) -> Result<&'a [Value], CapabilityError> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(CapabilityError::InvalidDescription(field))
}

fn instance_id(value: &Value) -> Result<u32, CapabilityError> {
    value
        .get("iid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(CapabilityError::InvalidDescription("iid"))
}

#[cfg(test)]
mod tests;
