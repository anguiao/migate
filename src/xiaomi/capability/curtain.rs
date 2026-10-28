use super::{
    CapabilityError, CurtainBindings, CurtainMotorBinding, CurtainPositionBinding,
    CurtainStatusBinding, NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding,
    description::{Description, Property, Service, Urn, array, string},
};
use crate::device::{DeviceKey, NumericRange};

pub(crate) fn is_curtain_type(value: &str) -> bool {
    Urn::parse(value).is_ok_and(|urn| urn.standard("device", "curtain", 0x0000a00c))
}

pub(super) fn parse_curtain(
    key: DeviceKey,
    instance: &Description<'_>,
) -> Result<ParsedDevice, CapabilityError> {
    let services = instance
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "curtain", 0x7816))
        .collect::<Vec<_>>();
    let [service] = services.as_slice() else {
        return Err(CapabilityError::InvalidDescription("curtain service"));
    };
    let motor = property(service, "motor-control", 0x38)?;
    let status = property(service, "status", 0x07)?;
    let current = property(service, "current-position", 0x39)?;
    let target = property(service, "target-position", 0x3a)?;
    access(motor, &["write"])?;
    access(status, &["read", "notify"])?;
    access(current, &["read", "notify"])?;
    access(target, &["read", "write", "notify"])?;
    let binding = |property: &Property<'_>| PropertyBinding {
        siid: service.iid,
        piid: property.iid,
    };
    let curtain = CurtainBindings::new(
        CurtainMotorBinding::new(binding(motor), format(motor)?, enum_value(motor, "pause")?)?,
        CurtainStatusBinding::new(
            binding(status),
            format(status)?,
            enum_value(status, "opening")?,
            enum_value(status, "closing")?,
            enum_value(status, "stop")?,
        )?,
        position(binding(current), current)?,
        position(binding(target), target)?,
    );
    ParsedDevice::new(
        key,
        vec![ParsedFunction::curtain_control("main", "窗帘", curtain)],
        None,
    )
}

fn property<'a>(
    service: &'a Service<'a>,
    name: &'static str,
    type_id: u32,
) -> Result<&'a Property<'a>, CapabilityError> {
    let mut candidates = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id));
    let first = candidates
        .next()
        .ok_or(CapabilityError::MissingFunction(name))?;
    if candidates.next().is_some() {
        return Err(CapabilityError::InvalidDescription(
            "ambiguous curtain property",
        ));
    }
    Ok(first)
}
fn access(property: &Property<'_>, required: &[&'static str]) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?;
    if access.iter().any(|item| item.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("access"));
    }
    for permission in required {
        if !access.iter().any(|item| item.as_str() == Some(*permission)) {
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
        _ => Err(CapabilityError::InvalidDescription("curtain format")),
    }
}
fn enum_value(property: &Property<'_>, name: &str) -> Result<u32, CapabilityError> {
    let values = array(property.value, "value-list")?;
    let mut candidates = values.iter().filter(|item| {
        item.get("description")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|description| description.eq_ignore_ascii_case(name))
    });
    let item = candidates
        .next()
        .ok_or(CapabilityError::InvalidDescription("curtain enumeration"))?;
    if candidates.next().is_some() {
        return Err(CapabilityError::InvalidDescription("curtain enumeration"));
    }
    let value = item
        .get("value")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(CapabilityError::InvalidDescription("curtain enumeration"))?;
    if values
        .iter()
        .filter(|item| {
            item.get("value").and_then(serde_json::Value::as_u64) == Some(u64::from(value))
        })
        .count()
        != 1
    {
        return Err(CapabilityError::InvalidDescription("curtain enumeration"));
    }
    Ok(value)
}
fn position(
    binding: PropertyBinding,
    property: &Property<'_>,
) -> Result<CurtainPositionBinding, CapabilityError> {
    if string(property.value, "unit")? != "percentage" {
        return Err(CapabilityError::InvalidDescription("curtain position unit"));
    }
    let values = array(property.value, "value-range")?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(CapabilityError::InvalidDescription(
                    "curtain position range",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [min, max, step] = values.as_slice() else {
        return Err(CapabilityError::InvalidDescription(
            "curtain position range",
        ));
    };
    let range = NumericRange::new(*min, *max, *step)
        .map_err(|_| CapabilityError::InvalidDescription("curtain position range"))?;
    CurtainPositionBinding::new(binding, format(property)?, range)
}
