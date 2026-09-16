use super::{CompileError, WireValue};
use crate::device::{DeviceCommand, HvacMode, Property, PropertyValue, SwingMode};

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyMiioOperation {
    pub method: &'static str,
    pub arguments: Vec<WireValue>,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Mcn02LegacyMapping;
impl Mcn02LegacyMapping {
    pub fn new() -> Self {
        Self
    }
    pub fn read_fields(&self) -> &'static [&'static str] {
        &["power", "mode", "tar_temp", "fan_level", "ver_swing"]
    }
    pub fn decode(
        &self,
        field: &str,
        value: &WireValue,
    ) -> Option<(Property, Option<PropertyValue>)> {
        let decoded = match field {
            "power" => bool_or_on_off(value).map(PropertyValue::Power),
            "mode" => string_value(value)
                .and_then(hvac_mode)
                .map(PropertyValue::HvacMode),
            "tar_temp" => number_value(value)
                .filter(|value| (16.0..=30.0).contains(value) && value.fract().abs() < 1e-9)
                .map(PropertyValue::Temperature),
            "fan_level" => string_value(value)
                .and_then(|value| match value {
                    "auto_fan" => Some(0),
                    "small_fan" => Some(1),
                    "medium_fan" => Some(2),
                    "large_fan" => Some(3),
                    _ => None,
                })
                .map(PropertyValue::FanSpeed),
            "ver_swing" => bool_or_on_off(value).map(|value| {
                PropertyValue::SwingMode(if value {
                    SwingMode::Vertical
                } else {
                    SwingMode::Off
                })
            }),
            _ => return None,
        };
        let property = match field {
            "power" => Property::Power,
            "mode" => Property::HvacMode,
            "tar_temp" => Property::TargetTemperature,
            "fan_level" => Property::FanSpeed,
            "ver_swing" => Property::SwingMode,
            _ => return None,
        };
        Some((property, decoded))
    }
    pub fn encode(&self, command: &DeviceCommand) -> Result<LegacyMiioOperation, CompileError> {
        let (method, value) = match command {
            DeviceCommand::SetPower(value) => (
                "set_power",
                WireValue::String(if *value { "on" } else { "off" }.into()),
            ),
            DeviceCommand::SetHvacMode(value) => (
                "set_mode",
                WireValue::String(
                    match value {
                        HvacMode::Auto => "auto",
                        HvacMode::Cool => "cool",
                        HvacMode::Dry => "dry",
                        HvacMode::Heat => "heat",
                        HvacMode::FanOnly => "wind",
                        HvacMode::Off => return Err(CompileError::InvalidValue),
                    }
                    .into(),
                ),
            ),
            DeviceCommand::SetTargetTemperature(value)
                if (16.0..=30.0).contains(value) && value.fract().abs() < 1e-9 =>
            {
                ("set_tar_temp", WireValue::Integer(*value as i64))
            }
            DeviceCommand::SetTargetTemperature(_) => return Err(CompileError::InvalidValue),
            DeviceCommand::SetFanSpeed(value) => (
                "set_fan_level",
                WireValue::String(
                    match value {
                        0 => "auto_fan",
                        1 => "small_fan",
                        2 => "medium_fan",
                        3 => "large_fan",
                        _ => return Err(CompileError::InvalidValue),
                    }
                    .into(),
                ),
            ),
            DeviceCommand::SetSwingMode(value) => (
                "set_ver_swing",
                WireValue::String(
                    if *value == SwingMode::Off {
                        "off"
                    } else if *value == SwingMode::Vertical {
                        "on"
                    } else {
                        return Err(CompileError::InvalidValue);
                    }
                    .into(),
                ),
            ),
            _ => return Err(CompileError::UnsupportedCommand),
        };
        Ok(LegacyMiioOperation {
            method,
            arguments: vec![value],
        })
    }
}
fn string_value(value: &WireValue) -> Option<&str> {
    if let WireValue::String(value) = value {
        Some(value)
    } else {
        None
    }
}
fn bool_or_on_off(value: &WireValue) -> Option<bool> {
    (if let WireValue::Boolean(value) = value {
        Some(*value)
    } else {
        None
    })
    .or_else(|| {
        string_value(value).and_then(|value| match value {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        })
    })
}

fn number_value(value: &WireValue) -> Option<f64> {
    match value {
        WireValue::Integer(value) => Some(*value as f64),
        WireValue::Number(value) if value.is_finite() => Some(*value),
        _ => None,
    }
}

fn hvac_mode(value: &str) -> Option<HvacMode> {
    match value {
        "auto" => Some(HvacMode::Auto),
        "cool" => Some(HvacMode::Cool),
        "dry" => Some(HvacMode::Dry),
        "heat" => Some(HvacMode::Heat),
        "wind" => Some(HvacMode::FanOnly),
        _ => None,
    }
}
