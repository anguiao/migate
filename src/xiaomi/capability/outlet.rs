use serde_json::Value;

use super::{
    CapabilityError, ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Property, Urn, array, string},
};
use crate::device::{Capability, Category, DeviceDescription, DeviceKey};

/// Parse stable outlet functions without treating vendor extensions as standard switches.
pub fn parse_device(key: DeviceKey, instance: &Value) -> Result<ParsedDevice, CapabilityError> {
    let instance = Description::parse(instance)?;
    if !instance.urn.standard("device", "outlet", 0x0000A002) {
        return Err(CapabilityError::UnsupportedDevice);
    }
    let description = DeviceDescription {
        key,
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    };
    if instance.urn.vendor == "cuco-cp7pd" {
        parse_cp7pd(description, &instance)
    } else {
        Ok(ParsedDevice {
            description,
            functions: vec![ParsedFunction {
                id: "main".into(),
                label: "主电源".into(),
                power: standard_main_power(&instance)?,
            }],
            refresh_trigger: None,
        })
    }
}

pub(crate) fn is_outlet_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "outlet", 0x0000A002))
}

fn standard_main_power(instance: &Description<'_>) -> Result<PropertyBinding, CapabilityError> {
    let candidates = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "switch", 0x0000780C))
        .flat_map(|service| {
            service
                .properties
                .iter()
                .filter(|property| property.urn.standard("property", "on", 0x00000006))
                .map(|property| (service.iid, property))
        })
        .collect::<Vec<_>>();
    let (siid, property) = match candidates.as_slice() {
        [] => return Err(CapabilityError::MissingMainPower),
        [candidate] => *candidate,
        _ => return Err(CapabilityError::AmbiguousMainPower),
    };
    power_binding(siid, property)
}

fn power_binding(siid: u32, property: &Property<'_>) -> Result<PropertyBinding, CapabilityError> {
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidMainPowerType);
    }
    let access = array(property.value, "access")?
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
    Ok(PropertyBinding {
        siid,
        piid: property.iid,
    })
}

fn parse_cp7pd(
    description: DeviceDescription,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    if instance.urn.version != 1 || instance.urn.suffix.is_some() {
        return Err(CapabilityError::IncompatibleModel("cuco.plug.cp7pd"));
    }
    let refresh_trigger = cp7pd_binding(instance, "master", 2, "miot-spec-v2", 0x0000780C)?;
    let mut functions = Vec::new();
    for (id, label, siid, namespace, type_id) in [
        ("outlet_1", "开关 1", 3, "cuco-spec", 0x00007801),
        ("outlet_2", "开关 2", 4, "cuco-spec", 0x00007802),
        ("usb", "USB", 9, "miot-spec-v2", 0x0000780C),
    ] {
        functions.push(ParsedFunction {
            id: id.into(),
            label: label.into(),
            power: cp7pd_binding(instance, id, siid, namespace, type_id)?,
        });
    }
    Ok(ParsedDevice {
        description,
        functions,
        refresh_trigger: Some(refresh_trigger),
    })
}

fn cp7pd_binding(
    instance: &Description<'_>,
    id: &'static str,
    siid: u32,
    namespace: &str,
    type_id: u32,
) -> Result<PropertyBinding, CapabilityError> {
    let service = instance
        .services
        .iter()
        .find(|service| service.iid == siid)
        .ok_or(CapabilityError::MissingFunction(id))?;
    if !cp7pd_urn(&service.urn, namespace, "service", "switch", type_id) {
        return Err(CapabilityError::IncompatibleModel("cuco.plug.cp7pd"));
    }
    let property = service
        .properties
        .iter()
        .find(|property| property.iid == 1)
        .ok_or(CapabilityError::MissingFunction(id))?;
    let property_id = if namespace == "cuco-spec" { 1 } else { 6 };
    if !cp7pd_urn(&property.urn, namespace, "property", "on", property_id) {
        return Err(CapabilityError::IncompatibleModel("cuco.plug.cp7pd"));
    }
    power_binding(siid, property)
}

fn cp7pd_urn(urn: &Urn<'_>, namespace: &str, entity: &str, name: &str, type_id: u32) -> bool {
    urn.matches(namespace, entity, name, type_id)
        && urn.vendor == "cuco-cp7pd"
        && urn.version == 1
        && urn.suffix.is_none()
}
