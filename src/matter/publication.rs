//! Pure Matter light publication mapping.
//!
//! The generic device model accepts every nonempty combination of on/off,
//! brightness, color temperature and RGB. Matter standard light types have
//! stricter requirements (all of them require OnOff and the color temperature
//! range must be representable in mireds), so this module owns the single
//! mapping decision shared by the Matter topology, storage and
//! the Xiaomi adapter publication eligibility. It is intentionally pure: it
//! only reads a [`FunctionDescription`] and returns a publication decision or
//! an explicit unmappable reason.

use crate::device::{Capability, FunctionDescription, NumericRange};
use rs_matter::dm::DeviceType;

/// The Matter device type selected for a mappable light.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LightDeviceType {
    /// On/Off Light (`0x0100`).
    OnOffLight,
    /// Dimmable Light (`0x0101`).
    DimmableLight,
    /// Color Temperature Light (`0x010C`).
    ColorTemperatureLight,
    /// Extended Color Light (`0x010D`).
    ExtendedColorLight,
}

impl LightDeviceType {
    pub(crate) const fn matter_device_type(self) -> DeviceType {
        match self {
            Self::OnOffLight => DeviceType {
                dtype: 0x0100,
                drev: 3,
            },
            Self::DimmableLight => DeviceType {
                dtype: 0x0101,
                drev: 3,
            },
            Self::ColorTemperatureLight => DeviceType {
                dtype: 0x010c,
                drev: 4,
            },
            Self::ExtendedColorLight => DeviceType {
                dtype: 0x010d,
                drev: 4,
            },
        }
    }
}

/// A physically well-formed temperature range together with its Matter mired
/// bounds, computed once so every layer uses the same representability result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublishedTemperature {
    pub range: NumericRange,
    pub bounds: (u16, u16),
}

/// The Matter surface of one mappable light.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LightPublication {
    pub device_type: LightDeviceType,
    pub brightness: bool,
    pub temperature: Option<PublishedTemperature>,
    pub rgb: bool,
}

impl LightPublication {
    pub(crate) const fn has_color_control(self) -> bool {
        self.temperature.is_some() || self.rgb
    }
}

/// Why a valid generic light cannot be published as a Matter light.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnmappableReason {
    /// The description is not a light function.
    NotLight,
    /// Every standard Matter light type requires a real OnOff capability.
    MissingOnOff,
    /// The real temperature range cannot be represented in Matter mireds.
    TemperatureRange,
}

impl UnmappableReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotLight => "device function is not a light",
            Self::MissingOnOff => "light has no on/off capability",
            Self::TemperatureRange => "temperature range cannot be represented in Matter mireds",
        }
    }
}

/// Map one function description to its Matter light publication.
///
/// The reported [`LightPublication`] never adds a capability the device does
/// not have; a light without OnOff or with an unrepresentable temperature
/// range is reported as [`UnmappableReason`] instead.
pub(crate) fn map_light(
    description: &FunctionDescription,
) -> Result<LightPublication, UnmappableReason> {
    use crate::device::Category;

    if description.category != Category::Light {
        return Err(UnmappableReason::NotLight);
    }
    if !description.capabilities.contains(&Capability::OnOff) {
        return Err(UnmappableReason::MissingOnOff);
    }
    let brightness = description.capabilities.contains(&Capability::Brightness);
    let rgb = description.capabilities.contains(&Capability::ColorRgb);
    let temperature = match description.temperature_range() {
        Some(range) => {
            let bounds = range
                .mired_bounds()
                .map_err(|_| UnmappableReason::TemperatureRange)?;
            Some(PublishedTemperature { range, bounds })
        }
        None => None,
    };
    let device_type = match (brightness, temperature.is_some(), rgb) {
        (true, true, true) => LightDeviceType::ExtendedColorLight,
        (true, true, false) => LightDeviceType::ColorTemperatureLight,
        (true, false, _) => LightDeviceType::DimmableLight,
        (false, _, _) => LightDeviceType::OnOffLight,
    };
    Ok(LightPublication {
        device_type,
        brightness,
        temperature,
        rgb,
    })
}
