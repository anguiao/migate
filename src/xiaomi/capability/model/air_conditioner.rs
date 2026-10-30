use super::{CapabilityError, NumericFormat, PropertyBinding};
use crate::device::air_conditioner::{AirConditionerFanSpeed, AirConditionerMode, TemperatureGrid};
use serde_json::Value;

fn valid_property(property: PropertyBinding) -> bool {
    property.siid != 0 && property.piid != 0
}

fn validate_codes<const N: usize>(
    property: PropertyBinding,
    format: NumericFormat,
    codes: [i64; N],
) -> Result<(), CapabilityError> {
    let Some((min, max)) = format.measurement_bounds() else {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner enum format",
        ));
    };
    if !valid_property(property)
        || codes
            .iter()
            .any(|code| (*code as f64) < min || (*code as f64) > max)
        || codes
            .iter()
            .enumerate()
            .any(|(i, code)| codes[..i].contains(code))
    {
        return Err(CapabilityError::InvalidDescription(
            "air-conditioner enum values",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerModeBinding {
    property: PropertyBinding,
    format: NumericFormat,
    codes: [i64; 5],
}
impl AirConditionerModeBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        codes: [i64; 5],
    ) -> Result<Self, CapabilityError> {
        validate_codes(property, format, codes)?;
        Ok(Self {
            property,
            format,
            codes,
        })
    }
    pub(crate) fn property(&self) -> PropertyBinding {
        self.property
    }
    pub(crate) fn format(&self) -> NumericFormat {
        self.format
    }
    pub(crate) fn codes(&self) -> [i64; 5] {
        self.codes
    }
    pub(crate) fn encode(&self, mode: AirConditionerMode) -> i64 {
        self.codes[mode.index()]
    }
    pub(crate) fn read_value(&self, value: &Value) -> Result<AirConditionerMode, CapabilityError> {
        let raw = value
            .as_i64()
            .ok_or(CapabilityError::InvalidValue("air-conditioner mode"))?;
        self.codes
            .iter()
            .position(|code| *code == raw)
            .map(|index| AirConditionerMode::ALL[index])
            .ok_or(CapabilityError::InvalidValue("air-conditioner mode"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerSpeedBinding {
    property: PropertyBinding,
    format: NumericFormat,
    codes: [i64; 4],
}
impl AirConditionerSpeedBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        codes: [i64; 4],
    ) -> Result<Self, CapabilityError> {
        validate_codes(property, format, codes)?;
        Ok(Self {
            property,
            format,
            codes,
        })
    }
    pub(crate) fn property(&self) -> PropertyBinding {
        self.property
    }
    pub(crate) fn format(&self) -> NumericFormat {
        self.format
    }
    pub(crate) fn codes(&self) -> [i64; 4] {
        self.codes
    }
    pub(crate) fn encode(&self, speed: AirConditionerFanSpeed) -> i64 {
        self.codes[speed.index()]
    }
    pub(crate) fn read_value(
        &self,
        value: &Value,
    ) -> Result<AirConditionerFanSpeed, CapabilityError> {
        let raw = value
            .as_i64()
            .ok_or(CapabilityError::InvalidValue("air-conditioner fan speed"))?;
        self.codes
            .iter()
            .position(|code| *code == raw)
            .map(|index| AirConditionerFanSpeed::ALL[index])
            .ok_or(CapabilityError::InvalidValue("air-conditioner fan speed"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerTemperatureBinding {
    property: PropertyBinding,
    format: NumericFormat,
    grid: TemperatureGrid,
}
impl AirConditionerTemperatureBinding {
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
                "air-conditioner temperature format",
            ));
        }
        Ok(Self {
            property,
            format,
            grid,
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
    /// Encode a target already checked against the effective constraints.
    pub(crate) fn encode(&self, temperature: i16) -> Value {
        debug_assert!(self.grid.contains(temperature));
        match self.format {
            NumericFormat::Float => serde_json::json!(f64::from(temperature)),
            _ => serde_json::json!(temperature),
        }
    }
    pub(crate) fn read_value(&self, value: &Value) -> Result<i16, CapabilityError> {
        let raw = value
            .as_f64()
            .ok_or(CapabilityError::InvalidValue("air-conditioner temperature"))?;
        if !raw.is_finite()
            || raw.fract() != 0.0
            || raw < f64::from(self.grid.min())
            || raw > f64::from(self.grid.max())
        {
            return Err(CapabilityError::InvalidValue("air-conditioner temperature"));
        }
        let value = raw as i16;
        if !self.grid.contains(value) {
            return Err(CapabilityError::InvalidValue("air-conditioner temperature"));
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerBindings {
    power: PropertyBinding,
    mode: AirConditionerModeBinding,
    temperature: AirConditionerTemperatureBinding,
    speed: AirConditionerSpeedBinding,
    swing: Option<PropertyBinding>,
}
impl AirConditionerBindings {
    pub(crate) fn new(
        power: PropertyBinding,
        mode: AirConditionerModeBinding,
        temperature: AirConditionerTemperatureBinding,
        speed: AirConditionerSpeedBinding,
        swing: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        let properties: Vec<_> = [
            Some(power),
            Some(mode.property()),
            Some(temperature.property()),
            Some(speed.property()),
            swing,
        ]
        .into_iter()
        .flatten()
        .collect();
        if properties.iter().any(|property| !valid_property(*property))
            || properties
                .iter()
                .enumerate()
                .any(|(i, property)| properties[..i].contains(property))
            || mode.property().siid != power.siid
            || temperature.property().siid != power.siid
            || speed.property().siid == power.siid
            || swing.is_some_and(|property| property.siid != speed.property().siid)
        {
            return Err(CapabilityError::InvalidDescription(
                "air-conditioner property bindings",
            ));
        }
        Ok(Self {
            power,
            mode,
            temperature,
            speed,
            swing,
        })
    }
    pub(crate) fn function_id(&self) -> String {
        format!("air-conditioner:{}", self.power.siid)
    }
    pub(crate) fn power(&self) -> PropertyBinding {
        self.power
    }
    pub(crate) fn mode(&self) -> &AirConditionerModeBinding {
        &self.mode
    }
    pub(crate) fn temperature(&self) -> &AirConditionerTemperatureBinding {
        &self.temperature
    }
    pub(crate) fn speed(&self) -> &AirConditionerSpeedBinding {
        &self.speed
    }
    pub(crate) fn swing(&self) -> Option<PropertyBinding> {
        self.swing
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        [
            Some(self.power),
            Some(self.mode.property()),
            Some(self.temperature.property()),
            Some(self.speed.property()),
            self.swing,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}
