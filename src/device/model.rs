use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceKey {
    pub source: String,
    pub id: String,
}

impl DeviceKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Outlet,
    Light,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    OnOff,
    Brightness,
    ColorTemperature(NumericRange),
    ColorRgb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Power {
    Unknown,
    On,
    Off,
}

impl Power {
    pub fn value(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }
}

impl From<bool> for Power {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    Initializing,
    Available,
    Unavailable(String),
}

impl Availability {
    pub(super) fn command_result(&self) -> Result<(), CommandError> {
        match self {
            Self::Available => Ok(()),
            Self::Initializing => Err(CommandError::Unavailable("initializing".into())),
            Self::Unavailable(reason) => Err(CommandError::Unavailable(reason.clone())),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionSnapshot {
    pub power: Power,
    pub lighting: Option<LightSnapshot>,
    pub availability: Availability,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unsupported,
    Unavailable(String),
    UnknownState,
    Timeout,
    Rejected(String),
    SendFailed(String),
    Cancelled,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("device does not support on/off commands"),
            Self::Unavailable(reason) => write!(formatter, "device unavailable: {reason}"),
            Self::UnknownState => formatter.write_str("cannot toggle an unknown power state"),
            Self::Timeout => formatter.write_str("command acceptance timed out"),
            Self::Rejected(reason) => write!(formatter, "command rejected: {reason}"),
            Self::SendFailed(reason) => write!(formatter, "command send failed: {reason}"),
            Self::Cancelled => formatter.write_str("command cancelled"),
        }
    }
}

impl Error for CommandError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FunctionKey {
    pub device: DeviceKey,
    pub id: String,
}

impl FunctionKey {
    pub fn new(device: DeviceKey, id: impl Into<String>) -> Self {
        Self {
            device,
            id: id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FunctionDescription {
    pub id: String,
    pub label: String,
    pub category: Category,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalDeviceDescription {
    pub key: DeviceKey,
    pub label: String,
    pub functions: Vec<FunctionDescription>,
}

/// An integer physical range. Every accepted value lies on its declared grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NumericRange {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}
impl NumericRange {
    pub fn valid(self) -> bool {
        self.min < self.max && self.step > 0 && self.step <= self.max - self.min
    }
    pub fn contains(self, value: u32) -> bool {
        self.valid()
            && (self.min..=self.max).contains(&value)
            && (value - self.min).is_multiple_of(self.step)
    }
    pub fn effective_max(self) -> u32 {
        self.min + (self.max - self.min) / self.step * self.step
    }
    pub fn nonzero_bounds(self) -> Option<(u32, u32)> {
        if !self.valid() {
            return None;
        }
        let min = if self.min == 0 { self.step } else { self.min };
        let max = self.effective_max();
        (min < max).then_some((min, max))
    }
    /// Clamp and round to the nearest reachable value, without exceeding max.
    pub fn quantize(self, value: f64) -> u32 {
        let last = (self.max - self.min) / self.step;
        let index = ((value.clamp(self.min as f64, self.max as f64) - self.min as f64)
            / self.step as f64)
            .round() as u32;
        self.min + index.min(last) * self.step
    }
    pub fn mired_bounds(self) -> (u16, u16) {
        (
            (1_000_000.0 / self.effective_max() as f64).ceil() as u16,
            (1_000_000.0 / self.min as f64).floor() as u16,
        )
    }
    pub fn kelvin_from_mireds(self, mireds: u16) -> Result<u32, CommandError> {
        if mireds == 0 {
            return Err(CommandError::Rejected(
                "color temperature must be nonzero".into(),
            ));
        }
        Ok(self.quantize(1_000_000.0 / mireds as f64))
    }
    pub fn mireds_from_kelvin(self, kelvin: u32) -> Result<u16, CommandError> {
        if !self.contains(kelvin) {
            return Err(CommandError::Rejected(
                "color temperature is outside its physical range".into(),
            ));
        }
        let (min, max) = self.mired_bounds();
        Ok(((1_000_000.0 / kelvin as f64).round() as u16).clamp(min, max))
    }
}

/// Nonzero brightness uses [1, 100] percent; zero is handled by command semantics.
pub fn brightness_from_matter_level(level: u8) -> Result<f64, CommandError> {
    if !(1..=254).contains(&level) {
        return Err(CommandError::Rejected(
            "brightness level is outside 1..254".into(),
        ));
    }
    Ok(1.0 + (level as f64 - 1.0) * 99.0 / 253.0)
}
pub fn brightness_to_matter_level(percent: f64) -> Result<u8, CommandError> {
    validate_brightness(percent)?;
    Ok((1.0 + (percent - 1.0) * 253.0 / 99.0).round() as u8)
}
pub(crate) fn validate_brightness(percent: f64) -> Result<(), CommandError> {
    if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
        return Err(CommandError::Rejected(
            "brightness is outside 1..100 percent".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}
impl Rgb {
    pub fn from_packed(value: u32) -> Option<Self> {
        (value <= 0xffffff).then_some(Self {
            red: (value >> 16) as u8,
            green: (value >> 8) as u8,
            blue: value as u8,
        })
    }
    pub fn packed(self) -> u32 {
        (self.red as u32) << 16 | (self.green as u32) << 8 | self.blue as u32
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightMode {
    Temperature,
    Rgb,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightSnapshot {
    pub brightness_percent: Option<f64>,
    pub temperature_kelvin: Option<u32>,
    pub color: Option<Rgb>,
    pub mode: Option<LightMode>,
}
/// A single accepted report; omitted properties preserve existing observations.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LightPatch {
    pub power: Option<bool>,
    pub brightness_percent: Option<f64>,
    pub temperature_kelvin: Option<u32>,
    pub color: Option<Rgb>,
    pub mode: Option<LightMode>,
}
impl FunctionDescription {
    pub fn temperature_range(&self) -> Option<NumericRange> {
        self.capabilities
            .iter()
            .find_map(|capability| match capability {
                Capability::ColorTemperature(range) => Some(*range),
                _ => None,
            })
    }
    pub fn valid_capabilities(&self) -> bool {
        match self.category {
            Category::Outlet => self.capabilities == [Capability::OnOff],
            Category::Light => {
                let Some(range) = self.temperature_range() else {
                    return false;
                };
                range.valid()
                    && range.min > 0
                    && range.max <= 1_000_000
                    && (self.capabilities
                        == [
                            Capability::OnOff,
                            Capability::Brightness,
                            Capability::ColorTemperature(range),
                        ]
                        || self.capabilities
                            == [
                                Capability::OnOff,
                                Capability::Brightness,
                                Capability::ColorTemperature(range),
                                Capability::ColorRgb,
                            ])
            }
        }
    }
}
