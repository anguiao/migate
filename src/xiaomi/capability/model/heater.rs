use super::thermostat::valid_property;
use super::{CapabilityError, PropertyBinding, ThermostatTemperatureBinding};

/// A standard heating-only thermostat: one on/off feedback and one setpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeaterBindings {
    power: PropertyBinding,
    temperature: ThermostatTemperatureBinding,
    /// A real standard mode property kept only as a native diagnostic source.
    diagnostic_mode: Option<PropertyBinding>,
    /// A notification-only input that can change the real heater power.
    power_refresh_trigger: Option<PropertyBinding>,
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
        Ok(Self {
            power,
            temperature,
            diagnostic_mode: None,
            power_refresh_trigger: None,
        })
    }
    pub(crate) fn set_diagnostic_mode(
        &mut self,
        mode: PropertyBinding,
    ) -> Result<(), CapabilityError> {
        if !valid_property(mode) || mode == self.power || mode == self.temperature.property() {
            return Err(CapabilityError::InvalidDescription(
                "heater diagnostic mode binding",
            ));
        }
        self.diagnostic_mode = Some(mode);
        Ok(())
    }
    pub(crate) fn set_power_refresh_trigger(&mut self, trigger: Option<PropertyBinding>) {
        self.power_refresh_trigger = trigger;
    }
    pub(crate) fn power_refresh_trigger(&self) -> Option<PropertyBinding> {
        self.power_refresh_trigger
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
    pub(crate) fn diagnostic_mode(&self) -> Option<PropertyBinding> {
        self.diagnostic_mode
    }
    pub(crate) fn observation_bindings(&self) -> Vec<PropertyBinding> {
        vec![self.power, self.temperature.property()]
    }
    pub(crate) fn diagnostic_bindings(&self) -> Vec<PropertyBinding> {
        self.diagnostic_mode.into_iter().collect()
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut bindings = self.observation_bindings();
        bindings.extend(self.diagnostic_mode);
        bindings.extend(self.power_refresh_trigger);
        bindings
    }
}
