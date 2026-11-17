use super::{
    CapabilityError,
    description::{Service, array, binding, string},
    model::ThermostatTemperatureBinding,
};
use crate::device::TemperatureGrid;

/// Parse a thermostat target whose range is expressed in whole Celsius degrees.
pub(super) fn target_temperature(
    service: &Service<'_>,
    allow_float: bool,
) -> Result<ThermostatTemperatureBinding, CapabilityError> {
    let property = service.property("target-temperature", 0x21)?;
    property.control_access()?;
    if string(property.value, "unit")? != "celsius" {
        return Err(CapabilityError::InvalidDescription(
            "thermostat temperature unit",
        ));
    }
    if property.value.get("value-list").is_some() {
        return Err(CapabilityError::InvalidDescription(
            "thermostat temperature shape",
        ));
    }
    let [min, max, step] = array(property.value, "value-range")? else {
        return Err(CapabilityError::InvalidDescription(
            "thermostat temperature range",
        ));
    };
    let grid = TemperatureGrid::new(integer(min)?, integer(max)?, integer(step)?)
        .map_err(|_| CapabilityError::InvalidDescription("thermostat temperature range"))?;
    let format = if allow_float {
        property.numeric_format()?
    } else {
        property.integer_format()?
    };
    ThermostatTemperatureBinding::new(binding(service, property), format, grid)
}

fn integer(value: &serde_json::Value) -> Result<i16, CapabilityError> {
    let raw = value.as_f64().ok_or(CapabilityError::InvalidDescription(
        "thermostat temperature range",
    ))?;
    if !raw.is_finite()
        || raw.fract() != 0.0
        || raw < f64::from(i16::MIN)
        || raw > f64::from(i16::MAX)
    {
        return Err(CapabilityError::InvalidDescription(
            "thermostat temperature range",
        ));
    }
    Ok(raw as i16)
}
