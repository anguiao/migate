use super::CapabilityError;
use crate::device::NumericRange;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericFormat {
    Uint8,
    Uint16,
    Uint32,
    Int8,
    Int16,
    Int32,
    Float,
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
            Self::Float => u32::MAX,
        }
    }
    pub(super) fn measurement_bounds(self) -> Option<(f64, f64)> {
        Some(match self {
            Self::Uint8 => (0.0, u8::MAX as f64),
            Self::Uint16 => (0.0, u16::MAX as f64),
            Self::Uint32 => (0.0, u32::MAX as f64),
            Self::Int8 => (i8::MIN as f64, i8::MAX as f64),
            Self::Int16 => (i16::MIN as f64, i16::MAX as f64),
            Self::Int32 => (i32::MIN as f64, i32::MAX as f64),
            Self::Float => return None,
        })
    }
}

pub(super) fn raw_integer(value: &Value) -> Result<u32, CapabilityError> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(CapabilityError::InvalidValue("type"))
}
pub(super) fn grid_raw(value: &Value, range: NumericRange) -> Result<u32, CapabilityError> {
    let raw = raw_integer(value)?;
    if !range.contains(raw) {
        return Err(CapabilityError::InvalidValue("range or step"));
    }
    Ok(raw)
}
