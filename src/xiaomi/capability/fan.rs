use super::{
    CapabilityError, FanBindings, FanModeBinding, FanSpeedBinding, NumericFormat, ParsedDevice,
    ParsedFunction, PropertyBinding,
    description::{Description, Property, Service, Urn, array, string},
};
use crate::device::DeviceKey;

pub(crate) fn is_fan_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "fan", 0x0000a005))
}

pub(crate) fn is_dmaker_fan_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| {
        urn.standard("device", "fan", 0x0000a005) && urn.vendor.starts_with("dmaker-")
    })
}

pub(super) fn parse_fan(
    key: DeviceKey,
    model: &str,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    if !model.starts_with("dmaker.") || !instance.urn.vendor.starts_with("dmaker-") {
        return Err(CapabilityError::UnsupportedDevice);
    }
    let services = instance
        .services
        .iter()
        .filter(|s| s.urn.standard("service", "fan", 0x7808))
        .collect::<Vec<_>>();
    let [service] = services.as_slice() else {
        return Err(CapabilityError::MissingFunction("fan"));
    };
    let power = required(service, |p| p.urn.standard("property", "on", 6), "on")?;
    boolean(power)?;
    let mode = required(service, |p| p.urn.standard("property", "mode", 8), "mode")?;
    access(mode)?;
    let mode_format = format(mode)?;
    let values = array(mode.value, "value-list")?;
    let mut straight = None;
    let mut native_values = Vec::new();
    for item in values {
        let value = item
            .get("value")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CapabilityError::InvalidDescription("fan mode enumeration"))?;
        let name = string(item, "description")?;
        if name.eq_ignore_ascii_case("Straight Wind") && straight.replace(value).is_some() {
            return Err(CapabilityError::InvalidDescription("fan mode enumeration"));
        }
        native_values.push(value);
    }
    let mode = FanModeBinding::new(
        binding(service, mode),
        mode_format,
        straight.ok_or(CapabilityError::InvalidDescription("fan straight mode"))?,
        native_values,
    )?;
    let swings = service
        .properties
        .iter()
        .filter(|p| p.urn.standard("property", "horizontal-swing", 0x17))
        .collect::<Vec<_>>();
    let swing = match swings.as_slice() {
        [swing] if boolean(swing).is_ok() => Some(binding(service, swing)),
        _ => None,
    };
    let services = instance
        .services
        .iter()
        .filter(|s| {
            s.urn
                .matches("dmaker-spec", "service", "dm-service", 0x7802)
                && s.urn.vendor == instance.urn.vendor
        })
        .collect::<Vec<_>>();
    let [vendor] = services.as_slice() else {
        return Err(CapabilityError::MissingFunction("dm-service"));
    };
    let speed = required(
        vendor,
        |p| {
            p.urn.matches("dmaker-spec", "property", "speed-level", 1)
                && p.urn.vendor == instance.urn.vendor
        },
        "speed-level",
    )?;
    access(speed)?;
    let range = array(speed.value, "value-range")?;
    if range
        != [
            serde_json::json!(1),
            serde_json::json!(100),
            serde_json::json!(1),
        ]
    {
        return Err(CapabilityError::InvalidDescription("fan speed range"));
    }
    let speed = FanSpeedBinding::new(binding(vendor, speed), format(speed)?)?;
    let fan = FanBindings::new(binding(service, power), mode, speed, swing);
    ParsedDevice::new(
        key,
        vec![ParsedFunction::fan_control("main", "风扇", fan)],
        None,
    )
}
fn required<'a>(
    service: &'a Service<'a>,
    matches: impl Fn(&Property<'_>) -> bool,
    name: &'static str,
) -> Result<&'a Property<'a>, CapabilityError> {
    let properties = service
        .properties
        .iter()
        .filter(|property| matches(property))
        .collect::<Vec<_>>();
    match properties.as_slice() {
        [property] => Ok(property),
        [] => Err(CapabilityError::MissingFunction(name)),
        _ => Err(CapabilityError::InvalidDescription(
            "ambiguous fan property",
        )),
    }
}
fn binding(service: &Service<'_>, property: &Property<'_>) -> PropertyBinding {
    PropertyBinding {
        siid: service.iid,
        piid: property.iid,
    }
}
fn boolean(property: &Property<'_>) -> Result<(), CapabilityError> {
    access(property)?;
    if string(property.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidDescription("fan boolean format"));
    }
    Ok(())
}
fn access(property: &Property<'_>) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?;
    if access.iter().any(|item| item.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("access"));
    }
    for permission in ["read", "write", "notify"] {
        if !access.iter().any(|item| item.as_str() == Some(permission)) {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(())
}
fn format(property: &Property<'_>) -> Result<NumericFormat, CapabilityError> {
    match string(property.value, "format")? {
        "uint8" => Ok(NumericFormat::Uint8),
        "uint16" => Ok(NumericFormat::Uint16),
        "uint32" => Ok(NumericFormat::Uint32),
        "int8" => Ok(NumericFormat::Int8),
        "int16" => Ok(NumericFormat::Int16),
        "int32" => Ok(NumericFormat::Int32),
        _ => Err(CapabilityError::InvalidDescription("fan integer format")),
    }
}
