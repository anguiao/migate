use super::{CommandError, TemperatureInterval};

/// The four standard light profiles without their interval, for comparisons
/// that treat equivalent logical intervals independently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightProfileKind {
    OnOff,
    Dimmable,
    ColorTemperature,
    ExtendedColor,
}

/// The four standard light profiles. Color-temperature profiles carry their
/// valid logical interval; the extended profile additionally implies RGB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightProfile {
    OnOff,
    Dimmable,
    ColorTemperature(TemperatureInterval),
    ExtendedColor(TemperatureInterval),
}
impl LightProfile {
    pub fn kind(self) -> LightProfileKind {
        match self {
            Self::OnOff => LightProfileKind::OnOff,
            Self::Dimmable => LightProfileKind::Dimmable,
            Self::ColorTemperature(_) => LightProfileKind::ColorTemperature,
            Self::ExtendedColor(_) => LightProfileKind::ExtendedColor,
        }
    }
    pub fn temperature_interval(self) -> Option<TemperatureInterval> {
        match self {
            Self::ColorTemperature(interval) | Self::ExtendedColor(interval) => Some(interval),
            Self::OnOff | Self::Dimmable => None,
        }
    }
    pub fn has_brightness(self) -> bool {
        !matches!(self, Self::OnOff)
    }
    pub fn has_rgb(self) -> bool {
        matches!(self, Self::ExtendedColor(_))
    }
    /// A color temperature profile determines its mode; extended color needs feedback.
    pub fn inferred_color_mode(self) -> Option<LightMode> {
        match self {
            Self::ColorTemperature(_) => Some(LightMode::Temperature),
            Self::OnOff | Self::Dimmable | Self::ExtendedColor(_) => None,
        }
    }
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
/// Source report values, admitted as a logical observation before application.
/// Omitted properties preserve existing observations.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LightPatch {
    pub power: Option<bool>,
    pub brightness_percent: Option<f64>,
    pub temperature_kelvin: Option<u32>,
    pub color: Option<Rgb>,
    pub mode: Option<LightMode>,
}
