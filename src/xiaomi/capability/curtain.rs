use super::{
    CapabilityError, CurtainBindings, CurtainMotorBinding, CurtainPositionBinding,
    CurtainStatusBinding, ParsedFunction, PropertyBinding,
    description::{Property, Service, array, string},
};
use crate::device::NumericRange;

pub(super) fn parse_curtain_service(
    service: &Service<'_>,
) -> Result<ParsedFunction, CapabilityError> {
    let motor = service.property("motor-control", 0x38)?;
    let status = service.property("status", 0x07)?;
    let current = service.property("current-position", 0x39)?;
    let target = service.property("target-position", 0x3a)?;
    motor.access(&["write"])?;
    status.access(&["read", "notify"])?;
    current.access(&["read", "notify"])?;
    target.access(&["read", "write", "notify"])?;
    let binding = |property: &Property<'_>| PropertyBinding {
        siid: service.iid,
        piid: property.iid,
    };
    let curtain = CurtainBindings::new(
        CurtainMotorBinding::new(
            binding(motor),
            motor.integer_format()?,
            enum_value(motor, "pause")?,
        )?,
        CurtainStatusBinding::new(
            binding(status),
            status.integer_format()?,
            enum_value(status, "opening")?,
            enum_value(status, "closing")?,
            enum_value(status, "stop")?,
        )?,
        position(binding(current), current)?,
        position(binding(target), target)?,
    );
    Ok(ParsedFunction::curtain_control(
        format!("curtain:{}", service.iid),
        "窗帘",
        curtain,
    ))
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
    CurtainPositionBinding::new(binding, property.integer_format()?, range)
}
