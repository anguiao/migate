use super::{
    CapabilityError, NumericFormat, PropertyBinding,
    numeric::{grid_raw, raw_integer},
};
use crate::device::{LightMode, LightProfile, NumericRange, TemperatureInterval};
use serde_json::Value;

/// A native brightness grid. Construction guarantees a nonzero reachable
/// span that the declared format can hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrightnessBinding {
    property: PropertyBinding,
    format: NumericFormat,
    range: NumericRange,
}
impl BrightnessBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        range: NumericRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || range.nonzero_bounds().is_none()
            || range.max() > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("brightness range or format"));
        }
        Ok(Self {
            property,
            format,
            range,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn range(&self) -> NumericRange {
        self.range
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        grid_raw(value, self.range)
    }
    pub fn brightness_from_raw(&self, value: &Value) -> Result<f64, CapabilityError> {
        let raw = self.read_raw(value)?;
        if raw == 0 {
            return Ok(0.0);
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .expect("a valid brightness grid has a nonzero span");
        Ok(1.0 + (raw - min) as f64 * 99.0 / (max - min) as f64)
    }
    /// Convert an already validated brightness; zero uses a reachable zero.
    /// Quantization to the native grid stays at the Xiaomi source boundary.
    pub fn brightness_to_raw(&self, percent: f64) -> u32 {
        if percent == 0.0 && self.range.min() == 0 {
            return 0;
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .expect("a valid brightness grid has a nonzero span");
        self.range
            .quantize(min as f64 + (percent - 1.0) * (max - min) as f64 / 99.0)
            .max(min)
    }
}

/// A native color-temperature grid plus the logical interval it advertises.
/// The grid keeps its own step for Xiaomi quantization; the interval carries
/// no step and is what publication compares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemperatureBinding {
    property: PropertyBinding,
    format: NumericFormat,
    range: NumericRange,
    interval: TemperatureInterval,
}
impl TemperatureBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        range: NumericRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || range.min() == 0
            || range.max() > 1_000_000
            || range.max() > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("temperature range or format"));
        }
        let interval = TemperatureInterval::from_grid(range)
            .map_err(|_| CapabilityError::InvalidLight("temperature range or format"))?;
        Ok(Self {
            property,
            format,
            range,
            interval,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn range(&self) -> NumericRange {
        self.range
    }
    pub fn interval(&self) -> TemperatureInterval {
        self.interval
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        grid_raw(value, self.range)
    }
    /// Quantize a kelvin target onto the native grid. The target keeps full
    /// precision until this single source-boundary quantization.
    pub fn temperature_to_raw(&self, kelvin: f64) -> u32 {
        self.range.quantize(kelvin)
    }
}

/// A native RGB binding. The advertised range is fixed at `0..=0xffffff`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorBinding {
    property: PropertyBinding,
    format: NumericFormat,
}
impl ColorBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || format.maximum() < 0xffffff
        {
            return Err(CapabilityError::InvalidLight("rgb range or format"));
        }
        Ok(Self { property, format })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        let raw = raw_integer(value)?;
        if raw > 0xffffff {
            return Err(CapabilityError::InvalidValue("range or step"));
        }
        Ok(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeBinding {
    property: PropertyBinding,
    format: NumericFormat,
    temperature_value: u32,
    rgb_value: u32,
}
impl ModeBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        temperature_value: u32,
        rgb_value: u32,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || temperature_value == rgb_value
            || temperature_value > format.maximum()
            || rgb_value > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("mode"));
        }
        Ok(Self {
            property,
            format,
            temperature_value,
            rgb_value,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn temperature_value(&self) -> u32 {
        self.temperature_value
    }
    pub fn rgb_value(&self) -> u32 {
        self.rgb_value
    }
    pub fn read_mode(&self, value: &Value) -> Result<LightMode, CapabilityError> {
        match value.as_u64() {
            Some(value) if value == self.temperature_value as u64 => Ok(LightMode::Temperature),
            Some(value) if value == self.rgb_value as u64 => Ok(LightMode::Rgb),
            _ => Err(CapabilityError::InvalidValue("mode")),
        }
    }
}

/// Real light bindings in one of the four supported profiles. The stored
/// representation is the closed valid value; construction from the optional
/// real bindings is the only fallible step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightBindings {
    OnOff,
    Dimmable {
        brightness: BrightnessBinding,
    },
    ColorTemperature {
        brightness: BrightnessBinding,
        temperature: TemperatureBinding,
    },
    ExtendedColor {
        brightness: BrightnessBinding,
        temperature: TemperatureBinding,
        color: ColorBinding,
        mode: Option<ModeBinding>,
    },
}
impl LightBindings {
    pub(crate) fn new(
        brightness: Option<BrightnessBinding>,
        temperature: Option<TemperatureBinding>,
        color: Option<ColorBinding>,
        mode: Option<ModeBinding>,
    ) -> Result<Self, CapabilityError> {
        match (brightness, temperature, color, mode) {
            (None, None, None, None) => Ok(Self::OnOff),
            (Some(brightness), None, None, None) => Ok(Self::Dimmable { brightness }),
            (Some(brightness), Some(temperature), None, None) => Ok(Self::ColorTemperature {
                brightness,
                temperature,
            }),
            (Some(brightness), Some(temperature), Some(color), mode) => Ok(Self::ExtendedColor {
                brightness,
                temperature,
                color,
                mode,
            }),
            _ => Err(CapabilityError::InvalidLight(
                "unsupported light capability combination",
            )),
        }
    }
    pub fn brightness(&self) -> Option<&BrightnessBinding> {
        match self {
            Self::OnOff => None,
            Self::Dimmable { brightness }
            | Self::ColorTemperature { brightness, .. }
            | Self::ExtendedColor { brightness, .. } => Some(brightness),
        }
    }
    pub fn temperature(&self) -> Option<&TemperatureBinding> {
        match self {
            Self::OnOff | Self::Dimmable { .. } => None,
            Self::ColorTemperature { temperature, .. }
            | Self::ExtendedColor { temperature, .. } => Some(temperature),
        }
    }
    pub fn color(&self) -> Option<&ColorBinding> {
        match self {
            Self::ExtendedColor { color, .. } => Some(color),
            Self::OnOff | Self::Dimmable { .. } | Self::ColorTemperature { .. } => None,
        }
    }
    pub fn mode(&self) -> Option<&ModeBinding> {
        match self {
            Self::ExtendedColor { mode, .. } => mode.as_ref(),
            Self::OnOff | Self::Dimmable { .. } | Self::ColorTemperature { .. } => None,
        }
    }
    pub fn profile(&self) -> LightProfile {
        match self {
            Self::OnOff => LightProfile::OnOff,
            Self::Dimmable { .. } => LightProfile::Dimmable,
            Self::ColorTemperature { temperature, .. } => {
                LightProfile::ColorTemperature(temperature.interval())
            }
            Self::ExtendedColor { temperature, .. } => {
                LightProfile::ExtendedColor(temperature.interval())
            }
        }
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut bindings = Vec::new();
        if let Some(brightness) = self.brightness() {
            bindings.push(brightness.property());
        }
        if let Some(temperature) = self.temperature() {
            bindings.push(temperature.property());
        }
        if let Some(color) = self.color() {
            bindings.push(color.property());
        }
        if let Some(mode) = self.mode() {
            bindings.push(mode.property());
        }
        bindings
    }
}
