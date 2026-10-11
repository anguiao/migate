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
    pub power: Option<PropertyBinding>,
    pub lighting: Option<LightBindings>,
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
    fn valid(&self) -> bool {
        self.property.siid > 0
            && self.property.piid > 0
            && self.temperature_value != self.rgb_value
            && self.temperature_value <= self.format.maximum()
            && self.rgb_value <= self.format.maximum()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LightBindings {
    pub brightness: Option<NumericBinding>,
    pub temperature: Option<NumericBinding>,
    pub color: Option<NumericBinding>,
    pub mode: Option<ModeBinding>,
    /// Brightness conversion exists only with the brightness binding. Old
    /// caches always stored the enum and deserialize it as `Some`.
    pub brightness_conversion: Option<BrightnessConversion>,
}
impl LightBindings {
    pub(crate) fn has_dimensions(&self) -> bool {
        self.brightness.is_some() || self.temperature.is_some() || self.color.is_some()
    }
    pub(crate) fn valid(&self) -> bool {
        self.has_dimensions()
            && (self.brightness_conversion.is_some() == self.brightness.is_some())
            && self.brightness.as_ref().is_none_or(|brightness| {
                brightness.valid(NumericUnit::Percentage)
                    && brightness.range.nonzero_bounds().is_some()
            })
            && self.temperature.as_ref().is_none_or(|temperature| {
                temperature.valid(NumericUnit::Kelvin)
                    && temperature.range.min > 0
                    && temperature.range.max <= 1_000_000
            })
            && self.color.as_ref().is_none_or(|color| {
                color.valid(NumericUnit::Rgb)
                    && color.format == NumericFormat::Uint32
                    && color.range
                        == NumericRange {
                            min: 0,
                            max: 0xffffff,
                            step: 1,
                        }
            })
            && self.mode.as_ref().is_none_or(|mode| {
                // A CT/RGB switch only exists on a dual-color light; single
                // capability lights keep their mode intrinsic.
                self.temperature.is_some() && self.color.is_some() && mode.valid()
            })
    }
}
