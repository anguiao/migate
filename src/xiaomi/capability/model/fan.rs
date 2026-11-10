use super::{CapabilityError, NumericFormat, PropertyBinding, numeric::raw_integer};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FanSpeedEncoding {
    Range { min: i64, max: i64, step: i64 },
    Discrete { values: Vec<i64> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanSpeedBinding {
    property: PropertyBinding,
    format: NumericFormat,
    encoding: FanSpeedEncoding,
    count: u8,
}
impl FanSpeedBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        encoding: FanSpeedEncoding,
    ) -> Result<Self, CapabilityError> {
        let bounds = match format {
            NumericFormat::Uint8 => (0, u8::MAX as i64),
            NumericFormat::Uint16 => (0, u16::MAX as i64),
            NumericFormat::Uint32 => (0, u32::MAX as i64),
            NumericFormat::Int8 => (i8::MIN as i64, i8::MAX as i64),
            NumericFormat::Int16 => (i16::MIN as i64, i16::MAX as i64),
            NumericFormat::Int32 => (i32::MIN as i64, i32::MAX as i64),
            NumericFormat::Float => {
                return Err(CapabilityError::InvalidDescription("fan speed format"));
            }
        };
        let valid = |value: i64| (bounds.0..=bounds.1).contains(&value);
        if property.siid == 0 || property.piid == 0 {
            return Err(CapabilityError::InvalidDescription("fan speed property"));
        }
        let count = match &encoding {
            FanSpeedEncoding::Range { min, max, step } => {
                if !valid(*min) || !valid(*max) || min > max || *step <= 0 {
                    return Err(CapabilityError::InvalidDescription("fan speed range"));
                }
                (max - min) / step + 1
            }
            FanSpeedEncoding::Discrete { values } => {
                let mut unique = std::collections::HashSet::new();
                if values
                    .iter()
                    .any(|value| !valid(*value) || !unique.insert(*value))
                {
                    return Err(CapabilityError::InvalidDescription("fan speed enumeration"));
                }
                values.len() as i64
            }
        };
        if !(1..=100).contains(&count) {
            return Err(CapabilityError::InvalidDescription(
                "fan speed mapping requires 1..100 levels",
            ));
        }
        Ok(Self {
            property,
            format,
            encoding,
            count: count as u8,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub(crate) fn encoding(&self) -> &FanSpeedEncoding {
        &self.encoding
    }
    pub fn count(&self) -> u8 {
        self.count
    }
    pub fn read_value(&self, value: &Value) -> Result<u8, CapabilityError> {
        let raw = value
            .as_i64()
            .ok_or(CapabilityError::InvalidValue("fan speed"))?;
        let index = match &self.encoding {
            FanSpeedEncoding::Range { min, max, step }
                if (*min..=*max).contains(&raw) && (raw - min) % step == 0 =>
            {
                Some(((raw - min) / step) as usize)
            }
            FanSpeedEncoding::Range { .. } => None,
            FanSpeedEncoding::Discrete { values } => values.iter().position(|value| *value == raw),
        };
        index
            .map(|index| index as u8 + 1)
            .ok_or(CapabilityError::InvalidValue("fan speed"))
    }
    pub(crate) fn encode(&self, speed: u8) -> i64 {
        debug_assert!((1..=self.count).contains(&speed));
        let index = i64::from(speed - 1);
        match &self.encoding {
            FanSpeedEncoding::Range { min, step, .. } => min + index * step,
            FanSpeedEncoding::Discrete { values } => values[index as usize],
        }
    }
}

/// A no-argument MIoT action that is a real native operation, never a property.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBinding {
    pub siid: u32,
    pub aiid: u32,
}
impl ActionBinding {
    pub(crate) fn new(siid: u32, aiid: u32) -> Result<Self, CapabilityError> {
        if siid == 0 || aiid == 0 {
            return Err(CapabilityError::InvalidDescription("action binding"));
        }
        Ok(Self { siid, aiid })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum LegacyArgument {
    String(String),
    Integer(i64),
}

/// One built-in miIO command declared by a patch. It carries its native
/// method and arguments instead of pretending to be a property write.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCommandBinding {
    pub method: String,
    pub args: Vec<LegacyArgument>,
}
impl LegacyCommandBinding {
    pub(crate) fn new(
        method: impl Into<String>,
        args: Vec<LegacyArgument>,
    ) -> Result<Self, CapabilityError> {
        let method = method.into();
        if method.is_empty() || method.chars().any(char::is_control) {
            return Err(CapabilityError::InvalidDescription("legacy command"));
        }
        Ok(Self { method, args })
    }
    pub(crate) fn params(&self) -> Value {
        Value::Array(
            self.args
                .iter()
                .map(|argument| match argument {
                    LegacyArgument::String(value) => Value::String(value.clone()),
                    LegacyArgument::Integer(value) => Value::from(*value),
                })
                .collect(),
        )
    }
}

/// Which digit of the shared `fan_speed_idx` belongs to this fan.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedFanChannel {
    Blow,
    Ventilation,
}
/// A two-level fan whose selected level comes from the shared legacy feedback
/// group. High speed is the real boolean power property; low speed is a native
/// action or built-in command, so no synthetic speed property exists.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedFanSpeedBinding {
    pub channel: SharedFanChannel,
    pub cloud_action: Option<ActionBinding>,
    pub direct_command: Option<LegacyCommandBinding>,
}
impl SharedFanSpeedBinding {
    pub(crate) fn new(
        channel: SharedFanChannel,
        cloud_action: Option<ActionBinding>,
        direct_command: Option<LegacyCommandBinding>,
    ) -> Result<Self, CapabilityError> {
        if cloud_action.is_none() && direct_command.is_none() {
            return Err(CapabilityError::InvalidDescription(
                "shared fan speed has no low-level operation",
            ));
        }
        Ok(Self {
            channel,
            cloud_action,
            direct_command,
        })
    }
    pub const fn count(&self) -> u8 {
        2
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FanSpeedSource {
    Property(FanSpeedBinding),
    Shared(SharedFanSpeedBinding),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanBindings {
    power: PropertyBinding,
    mode: Option<FanModeBinding>,
    speed: FanSpeedSource,
    swing: Option<PropertyBinding>,
}
impl FanBindings {
    pub(crate) fn new(
        power: PropertyBinding,
        mode: Option<FanModeBinding>,
        speed: FanSpeedBinding,
        swing: Option<PropertyBinding>,
    ) -> Self {
        Self {
            power,
            mode,
            speed: FanSpeedSource::Property(speed),
            swing,
        }
    }
    pub(crate) fn shared(power: PropertyBinding, speed: SharedFanSpeedBinding) -> Self {
        Self {
            power,
            mode: None,
            speed: FanSpeedSource::Shared(speed),
            swing: None,
        }
    }
    pub fn power(&self) -> PropertyBinding {
        self.power
    }
    pub fn mode(&self) -> Option<&FanModeBinding> {
        self.mode.as_ref()
    }
    /// Only a standard native speed property can use this accessor.
    pub fn speed(&self) -> &FanSpeedBinding {
        match &self.speed {
            FanSpeedSource::Property(speed) => speed,
            FanSpeedSource::Shared(_) => {
                panic!("a shared feedback fan has no native speed property")
            }
        }
    }
    pub(crate) fn standard_speed(&self) -> Option<&FanSpeedBinding> {
        match &self.speed {
            FanSpeedSource::Property(speed) => Some(speed),
            FanSpeedSource::Shared(_) => None,
        }
    }
    pub(crate) fn shared_speed(&self) -> Option<&SharedFanSpeedBinding> {
        match &self.speed {
            FanSpeedSource::Property(_) => None,
            FanSpeedSource::Shared(speed) => Some(speed),
        }
    }
    pub fn speed_count(&self) -> u8 {
        match &self.speed {
            FanSpeedSource::Property(speed) => speed.count(),
            FanSpeedSource::Shared(speed) => speed.count(),
        }
    }
    pub fn swing(&self) -> Option<PropertyBinding> {
        self.swing
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        [
            Some(self.power),
            self.mode.as_ref().map(FanModeBinding::property),
            self.standard_speed().map(FanSpeedBinding::property),
            self.swing,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}
