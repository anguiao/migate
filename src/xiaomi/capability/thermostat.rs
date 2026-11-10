use super::{
    CapabilityError, NumericFormat,
    description::{Property, Service, array, binding, control_access, integer_format, string},
    model::ThermostatTemperatureBinding,
};
use crate::device::TemperatureGrid;

/// Parse the standard integer target-temperature property of a thermostat.
pub(super) fn target_temperature(
    service: &Service<'_>,
    allow_float: bool,
) -> Result<ThermostatTemperatureBinding, CapabilityError> {
    let property = unique_property(service, "target-temperature", 0x21)?;
    control_access(property)?;
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
    let format = if allow_float && string(property.value, "format")? == "float" {
        NumericFormat::Float
    } else {
        integer_format(property)?
    };
    ThermostatTemperatureBinding::new(binding(service, property), format, grid)
}

pub(super) fn unique_property<'a>(
    service: &'a Service<'a>,
    name: &'static str,
    type_id: u32,
) -> Result<&'a Property<'a>, CapabilityError> {
    let matches = service
        .properties
        .iter()
        .filter(|property| property.urn.standard("property", name, type_id))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [property] => Ok(*property),
        [] => Err(CapabilityError::MissingFunction(name)),
        _ => Err(CapabilityError::InvalidDescription(
            "ambiguous thermostat property",
        )),
    }
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
