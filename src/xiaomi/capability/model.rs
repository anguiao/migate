use std::{error::Error, fmt};

use crate::device::{DeviceKey, FunctionDescription, NumericRange};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ParsedFunction {
    pub description: FunctionDescription,
    pub power: PropertyBinding,
    pub lighting: Option<LightBindings>,
}
impl ParsedFunction {
    /// The single capability description shared by parsing, persistence and publication.
    pub fn capabilities(&self) -> Vec<crate::device::Capability> {
        let mut capabilities = vec![crate::device::Capability::OnOff];
        if let Some(light) = &self.lighting {
            if light.brightness.is_some() {
                capabilities.push(crate::device::Capability::Brightness);
            }
            if let Some(temperature) = &light.temperature {
                capabilities.push(crate::device::Capability::ColorTemperature(
                    temperature.range,
                ));
            }
            if light.color.is_some() {
                capabilities.push(crate::device::Capability::ColorRgb);
            }
        }
        capabilities
    }
    /// Fixed capability description plus one of the four standard light profiles.
    pub fn light_profile(&self) -> Result<crate::device::LightProfile, &'static str> {
        if self.capabilities() != self.description.capabilities {
            return Err("capabilities do not match the real bindings");
        }
        self.description.light_profile()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDevice {
    pub key: DeviceKey,
    pub functions: Vec<ParsedFunction>,
    pub refresh_trigger: Option<PropertyBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    UnsupportedLightGroup,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
    MissingFunction(&'static str),
    IncompatibleModel(&'static str),
    InvalidLight(&'static str),
    InvalidValue(&'static str),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::UnsupportedLightGroup => {
                formatter.write_str("Xiaomi light groups are not supported")
            }
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet switch must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "outlet switch lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
            Self::MissingFunction(id) => {
                write!(formatter, "required outlet binding is missing: {id}")
            }
            Self::InvalidLight(field) => write!(formatter, "invalid light capability: {field}"),
            Self::InvalidValue(field) => write!(formatter, "invalid light property value: {field}"),
            Self::IncompatibleModel(model) => {
                write!(formatter, "unsupported MIoT description for {model}")
            }
        }
    }
}

impl Error for CapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericFormat {
    Uint8,
    Uint16,
    Uint32,
    Int8,
    Int16,
    Int32,
}
impl NumericFormat {
    pub(super) fn maximum(self) -> u32 {
        match self {
            Self::Uint8 => u8::MAX.into(),
            Self::Uint16 => u16::MAX.into(),
            Self::Uint32 => u32::MAX,
            Self::Int8 => i8::MAX as u32,
            Self::Int16 => i16::MAX as u32,
            Self::Int32 => i32::MAX as u32,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericUnit {
    Percentage,
    Kelvin,
    Rgb,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NumericBinding {
    pub property: PropertyBinding,
    pub format: NumericFormat,
    pub unit: NumericUnit,
    pub range: NumericRange,
}
impl NumericBinding {
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        let raw = value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CapabilityError::InvalidValue("type"))?;
        if !self.range.contains(raw) {
            return Err(CapabilityError::InvalidValue("range or step"));
        }
        Ok(raw)
    }
    pub fn brightness_from_raw(&self, value: &Value) -> Result<f64, CapabilityError> {
        let raw = self.read_raw(value)?;
        if raw == 0 {
            return Ok(0.0);
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .ok_or(CapabilityError::InvalidValue("brightness range"))?;
        Ok(1.0 + (raw - min) as f64 * 99.0 / (max - min) as f64)
    }
    pub fn brightness_to_raw(&self, percent: f64) -> Result<u32, CapabilityError> {
        if percent == 0.0 && self.range.contains(0) {
            return Ok(0);
        }
        if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
            return Err(CapabilityError::InvalidValue("brightness"));
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .ok_or(CapabilityError::InvalidValue("brightness range"))?;
        Ok(self
            .range
            .quantize(min as f64 + (percent - 1.0) * (max - min) as f64 / 99.0)
            .max(min))
    }
    pub fn temperature_to_raw(&self, kelvin: u32) -> Result<u32, CapabilityError> {
        if kelvin == 0 {
            return Err(CapabilityError::InvalidValue("temperature"));
        }
        Ok(self.range.quantize(kelvin as f64))
    }
    pub(crate) fn valid(&self, unit: NumericUnit) -> bool {
        self.property.siid > 0
            && self.property.piid > 0
            && self.unit == unit
            && self.range.valid()
            && self.range.max <= self.format.maximum()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrightnessConversion {
    NonzeroLinear,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModeBinding {
    pub property: PropertyBinding,
    pub format: NumericFormat,
    pub temperature_value: u32,
    pub rgb_value: u32,
}
impl ModeBinding {
    pub fn read_mode(&self, value: &Value) -> Result<crate::device::LightMode, CapabilityError> {
        match value.as_u64() {
            Some(value) if value == self.temperature_value as u64 => {
                Ok(crate::device::LightMode::Temperature)
            }
            Some(value) if value == self.rgb_value as u64 => Ok(crate::device::LightMode::Rgb),
            _ => Err(CapabilityError::InvalidValue("mode")),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LightBindings {
    pub brightness: Option<NumericBinding>,
    pub temperature: Option<NumericBinding>,
    pub color: Option<NumericBinding>,
    pub mode: Option<ModeBinding>,
    pub brightness_conversion: BrightnessConversion,
}
impl LightBindings {
    pub(crate) fn valid(&self) -> bool {
        let brightness = self.brightness.as_ref().is_none_or(|binding| {
            binding.valid(NumericUnit::Percentage) && binding.range.nonzero_bounds().is_some()
        });
        let temperature = self.temperature.as_ref().is_none_or(|binding| {
            binding.valid(NumericUnit::Kelvin)
                && binding.range.min > 0
                && binding.range.max <= 1_000_000
                && binding.range.mired_bounds().is_ok()
        });
        let color = self.color.as_ref().is_none_or(|binding| {
            binding.valid(NumericUnit::Rgb)
                && binding.range
                    == NumericRange {
                        min: 0,
                        max: 0xffffff,
                        step: 1,
                    }
        });
        let mode = match &self.mode {
            None => true,
            Some(mode) => {
                self.temperature.is_some()
                    && self.color.is_some()
                    && mode.property.siid > 0
                    && mode.property.piid > 0
                    && mode.temperature_value != mode.rgb_value
                    && mode.temperature_value <= mode.format.maximum()
                    && mode.rgb_value <= mode.format.maximum()
            }
        };
        brightness && temperature && color && mode
    }
}
