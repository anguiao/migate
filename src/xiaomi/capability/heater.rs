use super::{
    CapabilityError, HeaterBindings, description::string, thermostat::target_temperature,
    thermostat::unique_property,
};

/// Recognize the standard PTC heater's independent heating switch and setpoint.
pub(super) fn parse_heater_service(
    service: &super::description::Service<'_>,
) -> Result<HeaterBindings, CapabilityError> {
    let power = unique_property(service, "heating", 0xc7)?;
    super::description::control_access(power)?;
    if string(power.value, "format")? != "bool" {
        return Err(CapabilityError::InvalidDescription("heater boolean format"));
    }
    let temperature = target_temperature(service, false)?;
    HeaterBindings::new(super::description::binding(service, power), temperature)
}
