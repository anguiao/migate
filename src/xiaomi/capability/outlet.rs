use super::{
    CapabilityError, ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Property, Urn, array, string},
};
use crate::device::DeviceKey;

/// Parse stable outlet functions without treating vendor extensions as standard switches.
pub(super) fn parse_outlet(
    key: DeviceKey,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    if !instance.urn.standard("device", "outlet", 0x0000A002) {
        return Err(CapabilityError::UnsupportedDevice);
    }
    if instance.urn.vendor == "cuco-cp7pd" {
        parse_cp7pd(key, instance)
    } else {
        ParsedDevice::new(
            key,
            vec![ParsedFunction::outlet(
                "main",
                "主电源",
                standard_main_power(instance)?,
            )],
            None,
        )
    }
}

pub(crate) fn is_outlet_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "outlet", 0x0000A002))
}

pub(crate) const CP7PD_MODEL: &str = "cuco.plug.cp7pd";

pub(crate) fn is_cp7pd_type(value: &str) -> bool {
    Urn::parse(value)
        .is_ok_and(|urn| urn.standard("device", "outlet", 0x0000A002) && urn.vendor == "cuco-cp7pd")
}

/// The exact cp7pd channel bindings and master trigger in their fixed layout.
pub(crate) fn is_cp7pd_topology(device: &ParsedDevice) -> bool {
    const CHANNELS: [(&str, PropertyBinding); 3] = [
        ("outlet_1", PropertyBinding { siid: 3, piid: 1 }),
        ("outlet_2", PropertyBinding { siid: 4, piid: 1 }),
        ("usb", PropertyBinding { siid: 9, piid: 1 }),
    ];
    if device.refresh_trigger() != Some(PropertyBinding { siid: 2, piid: 1 })
        || device
            .functions()
            .iter()
            .filter(|function| function.kind().is_control())
            .count()
            != CHANNELS.len()
    {
        return false;
    }
    device
        .functions()
        .iter()
        .filter(|function| function.kind().is_control())
        .zip(CHANNELS)
        .all(|(function, (id, power))| {
            function.lighting().is_none() && function.id() == id && function.power() == Some(power)
        })
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

pub(super) fn power_binding(
    siid: u32,
    property: &Property<'_>,
) -> Result<PropertyBinding, CapabilityError> {
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
    key: DeviceKey,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    let refresh_trigger = cp7pd_binding(instance, "master", 2, "miot-spec-v2", 0x0000780C)?;
    let mut functions = Vec::new();
    for (id, label, siid, namespace, type_id) in [
        ("outlet_1", "开关 1", 3, "cuco-spec", 0x00007801),
        ("outlet_2", "开关 2", 4, "cuco-spec", 0x00007802),
        ("usb", "USB", 9, "miot-spec-v2", 0x0000780C),
    ] {
        functions.push(ParsedFunction::outlet(
            id,
            label,
            cp7pd_binding(instance, id, siid, namespace, type_id)?,
        ));
    }
    ParsedDevice::new(key, functions, Some(refresh_trigger))
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
    urn.matches(namespace, entity, name, type_id) && urn.vendor == "cuco-cp7pd"
}
