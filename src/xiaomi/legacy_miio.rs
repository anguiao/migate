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
