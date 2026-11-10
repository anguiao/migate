use crate::xiaomi::capability::PropertyBinding;
use serde_json::{Value, json};
use std::fmt;

#[derive(Debug, Eq, PartialEq)]
pub(in crate::xiaomi) struct Mapping {
    pub(in crate::xiaomi) fields: &'static [Field],
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::xiaomi) struct Field {
    pub(in crate::xiaomi) binding: PropertyBinding,
    pub(in crate::xiaomi) name: &'static str,
    pub(in crate::xiaomi) setter: &'static str,
    pub(in crate::xiaomi) conversion: Conversion,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::xiaomi) enum Conversion {
    OnOff,
    Enum(&'static [&'static str]),
    Temperature { min: i64, max: i64 },
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::xiaomi) struct FeedbackError {
    field: &'static Field,
    value: Value,
}

impl fmt::Display for FeedbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid legacy miIO feedback for {}: expected ",
            self.field.name
        )?;
        match self.field.conversion {
            Conversion::OnOff => formatter.write_str("\"on\" or \"off\"")?,
            Conversion::Enum(names) => write!(formatter, "one of {names:?}")?,
            Conversion::Temperature { min, max } => {
                write!(formatter, "an integer from {min} to {max}")?;
            }
        }
        write!(formatter, ", received {}", self.value)
    }
}

impl Mapping {
    pub(in crate::xiaomi) fn fields(&self) -> Vec<&'static str> {
        self.fields.iter().map(|field| field.name).collect()
    }

    pub(in crate::xiaomi) fn contains(&self, binding: PropertyBinding) -> bool {
        self.fields.iter().any(|field| field.binding == binding)
    }

    pub(in crate::xiaomi) fn supports(&self, bindings: &[PropertyBinding]) -> bool {
        bindings.len() == self.fields.len()
            && bindings.iter().all(|binding| self.contains(*binding))
    }

    pub(in crate::xiaomi) fn encode(
        &self,
        binding: PropertyBinding,
        value: &Value,
    ) -> Option<(&'static str, Value)> {
        let field = self.fields.iter().find(|field| field.binding == binding)?;
        let native = match field.conversion {
            Conversion::OnOff => json!(if value.as_bool()? { "on" } else { "off" }),
            Conversion::Enum(names) => json!(*names.get(usize::try_from(value.as_i64()?).ok()?)?),
            Conversion::Temperature { min, max } => json!(temperature(value, min, max)?),
        };
        Some((field.setter, native))
    }

    pub(in crate::xiaomi) fn decode(
        &self,
        result: &Value,
    ) -> Result<Vec<Result<Value, FeedbackError>>, ()> {
        let values = result
            .as_array()
            .filter(|values| values.len() == self.fields.len())
            .ok_or(())?;
        Ok(self
            .fields
            .iter()
            .zip(values)
            .map(|(field, value)| {
                (match field.conversion {
                    Conversion::OnOff => match value.as_str() {
                        Some("on") => Ok(json!(true)),
                        Some("off") => Ok(json!(false)),
                        _ => Err(()),
                    },
                    Conversion::Enum(names) => names
                        .iter()
                        .position(|name| Some(*name) == value.as_str())
                        .map(|index| json!(index))
                        .ok_or(()),
                    Conversion::Temperature { min, max } => temperature(value, min, max)
                        .map(|value| json!(value))
                        .ok_or(()),
                })
                .map_err(|()| FeedbackError {
                    field,
                    value: value.clone(),
                })
            })
            .collect())
    }

    pub(in crate::xiaomi) fn index(&self, binding: PropertyBinding) -> Option<usize> {
        self.fields
            .iter()
            .position(|field| field.binding == binding)
    }
}

fn temperature(value: &Value, min: i64, max: i64) -> Option<i64> {
    let value = value.as_f64()?;
    (value.fract() == 0.0 && ((min as f64)..=(max as f64)).contains(&value)).then_some(value as i64)
}

/// One field of the shared legacy feedback group. `raw` is retained even when
/// the reported value is not usable, so diagnostics never erase the input.
pub(in crate::xiaomi) const SHARED_FIELDS: [&str; 3] = ["bh_mode", "fan_speed_idx", "temperature"];

#[derive(Clone, Debug, PartialEq)]
pub(in crate::xiaomi) struct SharedField<T> {
    pub(in crate::xiaomi) value: Option<T>,
    pub(in crate::xiaomi) raw: Value,
    pub(in crate::xiaomi) reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::xiaomi) struct SharedFanBit {
    /// Logical two-level fan speed: 1 = low, 2 = high.
    pub(in crate::xiaomi) level: Option<u8>,
    pub(in crate::xiaomi) reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::xiaomi) struct SharedFanSpeedReading {
    pub(in crate::xiaomi) raw: Value,
    pub(in crate::xiaomi) blow: SharedFanBit,
    pub(in crate::xiaomi) ventilation: SharedFanBit,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::xiaomi) struct SharedFeedbackReading {
    pub(in crate::xiaomi) native_mode: SharedField<String>,
    pub(in crate::xiaomi) fan_speed: SharedFanSpeedReading,
    pub(in crate::xiaomi) temperature: SharedField<f64>,
}

/// Validate the group shape at the transport boundary. A structurally
/// different response is a read failure; a well-shaped field with an invalid
/// value is an explicit unknown that only affects its consumers.
pub(in crate::xiaomi) fn decode_shared_feedback(
    result: &Value,
) -> Result<SharedFeedbackReading, ()> {
    let values = result
        .as_array()
        .filter(|values| values.len() == 3)
        .ok_or(())?;
    let native_mode = SharedField {
        value: values[0].as_str().map(str::to_owned),
        raw: values[0].clone(),
        reason: values[0]
            .as_str()
            .is_none()
            .then(|| format!("invalid bh_mode value {}", values[0])),
    };
    let fan_speed = decode_fan_speed(&values[1]);
    let temperature = match values[2].as_f64() {
        Some(value) if value.is_finite() => SharedField {
            value: Some(value),
            raw: values[2].clone(),
            reason: None,
        },
        _ => SharedField {
            value: None,
            raw: values[2].clone(),
            reason: Some(format!("invalid temperature value {}", values[2])),
        },
    };
    Ok(SharedFeedbackReading {
        native_mode,
        fan_speed,
        temperature,
    })
}

fn decode_fan_speed(raw: &Value) -> SharedFanSpeedReading {
    let Some(value) = raw.as_i64().filter(|value| *value >= 0) else {
        let reason = Some(format!("invalid fan_speed_idx value {raw}"));
        return SharedFanSpeedReading {
            raw: raw.clone(),
            blow: SharedFanBit {
                level: None,
                reason: reason.clone(),
            },
            ventilation: SharedFanBit {
                level: None,
                reason,
            },
        };
    };
    let value = value as u64;
    SharedFanSpeedReading {
        raw: raw.clone(),
        blow: decode_fan_bit((value / 10 % 10) as u32, "blow"),
        ventilation: decode_fan_bit((value % 10) as u32, "ventilation"),
    }
}

fn decode_fan_bit(code: u32, channel: &str) -> SharedFanBit {
    match code {
        1 => SharedFanBit {
            level: Some(1),
            reason: None,
        },
        3 => SharedFanBit {
            level: Some(2),
            reason: None,
        },
        0 => SharedFanBit {
            level: None,
            reason: Some(format!("{channel} speed code is 0")),
        },
        _ => SharedFanBit {
            level: None,
            reason: Some(format!("unrecognized {channel} speed code {code}")),
        },
    }
}
