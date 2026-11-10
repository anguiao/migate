use super::{CapabilityError, NumericFormat, PropertyBinding};
use crate::device::TemperatureGrid;
use serde_json::Value;

pub(super) fn valid_property(property: PropertyBinding) -> bool {
    property.siid != 0 && property.piid != 0
}

/// One integer target-temperature setpoint shared by every thermostat profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThermostatTemperatureBinding {
    property: PropertyBinding,
    format: NumericFormat,
    grid: TemperatureGrid,
    integer_encoding: bool,
}
impl ThermostatTemperatureBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        grid: TemperatureGrid,
    ) -> Result<Self, CapabilityError> {
        if !valid_property(property)
            || format.measurement_bounds().is_some_and(|(min, max)| {
                f64::from(grid.min()) < min || f64::from(grid.max()) > max
            })
        {
            return Err(CapabilityError::InvalidDescription(
                "thermostat temperature format",
            ));
        }
        Ok(Self {
            property,
            format,
            grid,
            integer_encoding: false,
        })
    }
    pub(crate) fn property(&self) -> PropertyBinding {
        self.property
    }
    pub(crate) fn format(&self) -> NumericFormat {
        self.format
    }
    pub(crate) fn grid(&self) -> TemperatureGrid {
        self.grid
    }
    pub(super) fn use_integer_encoding(&mut self) {
        self.integer_encoding = true;
    }
    /// Encode a target already checked against the effective constraints.
    pub(crate) fn encode(&self, temperature: i16) -> Value {
        debug_assert!(self.grid.contains(temperature));
        match self.format {
            NumericFormat::Float if !self.integer_encoding => {
                serde_json::json!(f64::from(temperature))
            }
            _ => serde_json::json!(temperature),
        }
    }
    pub(crate) fn read_value(&self, value: &Value) -> Result<i16, CapabilityError> {
        let raw = value
            .as_f64()
            .ok_or(CapabilityError::InvalidValue("thermostat temperature"))?;
        if !raw.is_finite()
            || raw.fract() != 0.0
            || raw < f64::from(self.grid.min())
            || raw > f64::from(self.grid.max())
        {
            return Err(CapabilityError::InvalidValue("thermostat temperature"));
        }
        let value = raw as i16;
        if !self.grid.contains(value) {
            return Err(CapabilityError::InvalidValue("thermostat temperature"));
        }
        Ok(value)
    }
}

pub(crate) type AirConditionerTemperatureBinding = ThermostatTemperatureBinding;
