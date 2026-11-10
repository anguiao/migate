pub(crate) use super::thermostat::AirConditionerTemperatureBinding;
use super::thermostat::valid_property;
use super::{CapabilityError, NumericFormat, PropertyBinding};
use crate::device::air_conditioner::{AirConditionerFanSpeed, AirConditionerMode};
use serde_json::Value;

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
pub(crate) struct Mcn04Bindings {
    pub(crate) state: PropertyBinding,
    pub(crate) range: PropertyBinding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerBindings {
    power: PropertyBinding,
    mode: AirConditionerModeBinding,
    temperature: AirConditionerTemperatureBinding,
    speed: AirConditionerSpeedBinding,
    swing: Option<PropertyBinding>,
    mcn04: Option<Mcn04Bindings>,
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
            mcn04: None,
        })
    }
    pub(crate) fn with_mcn04(
        mut self,
        state: PropertyBinding,
        range: PropertyBinding,
    ) -> Result<Self, CapabilityError> {
        if !valid_property(state)
            || !valid_property(range)
            || state == range
            || state.siid != range.siid
            || self.bindings().contains(&state)
            || self.bindings().contains(&range)
        {
            return Err(CapabilityError::InvalidDescription(
                "mcn04 feedback bindings",
            ));
        }
        self.temperature.use_integer_encoding();
        self.mcn04 = Some(Mcn04Bindings { state, range });
        Ok(self)
    }
    pub(crate) fn mcn04(&self) -> Option<Mcn04Bindings> {
        self.mcn04
    }
    pub(crate) fn observation_bindings(&self) -> Vec<PropertyBinding> {
        match self.mcn04 {
            Some(patch) => vec![patch.state, patch.range],
            None => self.bindings(),
        }
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
        .chain(
            self.mcn04
                .into_iter()
                .flat_map(|patch| [patch.state, patch.range]),
        )
        .collect()
    }
}
