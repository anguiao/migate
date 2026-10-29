use super::{CapabilityError, NumericFormat, PropertyBinding, numeric::raw_integer};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanModeBinding {
    property: PropertyBinding,
    format: NumericFormat,
    straight: u32,
    values: Vec<u32>,
}
impl FanModeBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        straight: u32,
        values: Vec<u32>,
    ) -> Result<Self, CapabilityError> {
        let mut unique = std::collections::HashSet::new();
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || values.is_empty()
            || !values.contains(&straight)
            || values
                .iter()
                .any(|value| *value > format.maximum() || !unique.insert(*value))
        {
            return Err(CapabilityError::InvalidDescription("fan mode enumeration"));
        }
        Ok(Self {
            property,
            format,
            straight,
            values,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn straight(&self) -> u32 {
        self.straight
    }
    pub fn values(&self) -> &[u32] {
        &self.values
    }
    pub fn read_value(&self, value: &Value) -> Result<u32, CapabilityError> {
        let value = raw_integer(value)?;
        if self.values.contains(&value) {
            Ok(value)
        } else {
            Err(CapabilityError::InvalidValue("fan mode"))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanSpeedBinding {
    property: PropertyBinding,
    format: NumericFormat,
}
impl FanSpeedBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || format.maximum() < 100
        {
            return Err(CapabilityError::InvalidDescription("fan speed format"));
        }
        Ok(Self { property, format })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn read_value(&self, value: &Value) -> Result<u8, CapabilityError> {
        let value = raw_integer(value)?;
        if (1..=100).contains(&value) {
            Ok(value as u8)
        } else {
            Err(CapabilityError::InvalidValue("fan speed"))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanBindings {
    power: PropertyBinding,
    mode: FanModeBinding,
    speed: FanSpeedBinding,
    swing: Option<PropertyBinding>,
}
impl FanBindings {
    pub(crate) fn new(
        power: PropertyBinding,
        mode: FanModeBinding,
        speed: FanSpeedBinding,
        swing: Option<PropertyBinding>,
    ) -> Self {
        Self {
            power,
            mode,
            speed,
            swing,
        }
    }
    pub fn power(&self) -> PropertyBinding {
        self.power
    }
    pub fn mode(&self) -> &FanModeBinding {
        &self.mode
    }
    pub fn speed(&self) -> &FanSpeedBinding {
        &self.speed
    }
    pub fn swing(&self) -> Option<PropertyBinding> {
        self.swing
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        [
            Some(self.power),
            Some(self.mode.property()),
            Some(self.speed.property()),
            self.swing,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}
