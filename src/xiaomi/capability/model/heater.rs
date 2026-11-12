use super::thermostat::valid_property;
use super::{CapabilityError, NumericFormat, PropertyBinding, ThermostatTemperatureBinding};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeaterFeedbackBinding {
    pub(crate) property: PropertyBinding,
    values: Vec<u32>,
}
impl HeaterFeedbackBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        values: Vec<u32>,
    ) -> Result<Self, CapabilityError> {
        if values.is_empty() || values.iter().any(|value| *value > format.maximum()) {
            return Err(CapabilityError::InvalidDescription(
                "heater feedback enumeration",
            ));
        }
        Ok(Self { property, values })
    }
    pub(crate) fn accepts(&self, raw: &Value) -> bool {
        super::numeric::raw_integer(raw).is_ok_and(|value| self.values.contains(&value))
    }
}

/// A standard heating-only thermostat: one on/off feedback and one setpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeaterBindings {
    power: PropertyBinding,
    temperature: ThermostatTemperatureBinding,
    /// A real standard mode property kept only as a native diagnostic source.
    diagnostic_mode: Option<HeaterFeedbackBinding>,
    /// A notification-only input that can change the real heater power.
    power_refresh_trigger: Option<HeaterFeedbackBinding>,
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
        mode: HeaterFeedbackBinding,
    ) -> Result<(), CapabilityError> {
        if !valid_property(mode.property)
            || mode.property == self.power
            || mode.property == self.temperature.property()
        {
            return Err(CapabilityError::InvalidDescription(
                "heater diagnostic mode binding",
            ));
        }
        self.diagnostic_mode = Some(mode);
        Ok(())
    }
    pub(crate) fn set_power_refresh_trigger(&mut self, trigger: HeaterFeedbackBinding) {
        self.power_refresh_trigger = Some(trigger);
    }
    pub(crate) fn power_refresh_trigger(&self) -> Option<&HeaterFeedbackBinding> {
        self.power_refresh_trigger.as_ref()
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
    pub(crate) fn diagnostic_mode(&self) -> Option<&HeaterFeedbackBinding> {
        self.diagnostic_mode.as_ref()
    }
    pub(crate) fn observation_bindings(&self) -> Vec<PropertyBinding> {
        vec![self.power, self.temperature.property()]
    }
    pub(crate) fn diagnostic_bindings(&self) -> Vec<PropertyBinding> {
        self.diagnostic_mode()
            .map(|mode| mode.property)
            .into_iter()
            .collect()
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut bindings = self.observation_bindings();
        bindings.extend(self.diagnostic_mode().map(|mode| mode.property));
        bindings.extend(self.power_refresh_trigger().map(|trigger| trigger.property));
        bindings
    }
}
