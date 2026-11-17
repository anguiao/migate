use super::{
    CapabilityError, HeaterBindings,
    description::{Service, binding},
    thermostat::target_temperature,
};

/// Recognize the standard PTC heater's independent heating switch and setpoint.
pub(super) fn parse_heater_service(
    service: &Service<'_>,
) -> Result<HeaterBindings, CapabilityError> {
    let power = service.property("heating", 0xc7)?;
    power.boolean_control()?;
    let temperature = target_temperature(service, false)?;
    HeaterBindings::new(binding(service, power), temperature)
}
