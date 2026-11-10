use super::thermostat::valid_property;
use super::{CapabilityError, PropertyBinding, ThermostatTemperatureBinding};

/// A standard heating-only thermostat: one on/off feedback and one setpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeaterBindings {
    power: PropertyBinding,
    temperature: ThermostatTemperatureBinding,
}
impl HeaterBindings {
    pub(crate) fn new(
        power: PropertyBinding,
        temperature: ThermostatTemperatureBinding,
    ) -> Result<Self, CapabilityError> {
        if !valid_property(power)
            || power.siid != temperature.property().siid
            || power == temperature.property()
        {
            return Err(CapabilityError::InvalidDescription(
                "heater property bindings",
            ));
        }
        Ok(Self { power, temperature })
    }
    pub(crate) fn function_id(&self) -> String {
        format!("heater:{}", self.power.siid)
    }
    pub(crate) fn power(&self) -> PropertyBinding {
        self.power
    }
    pub(crate) fn temperature(&self) -> &ThermostatTemperatureBinding {
        &self.temperature
    }
    pub(crate) fn observation_bindings(&self) -> Vec<PropertyBinding> {
        vec![self.power, self.temperature.property()]
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        self.observation_bindings()
    }
}
