//! Decoding of mcn04 source reports; native controls remain standard MIoT writes.
use crate::{
    device::{AirConditionerValue, air_conditioner::AirConditionerConstraints},
    xiaomi::capability::{AirConditionerBindings, CapabilityError},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct StateReport {
    pub(super) values: Vec<AirConditionerValue>,
    pub(super) problem: Option<String>,
}

pub(super) fn state(
    ac: &AirConditionerBindings,
    value: &Value,
) -> Result<StateReport, CapabilityError> {
    let text = value
        .as_str()
        .ok_or(CapabilityError::InvalidValue("mcn04 ac-state"))?;
    let mut fields = [None; 5];
    let mut duplicates = [false; 5];
    let mut errors = Vec::new();
    for part in text.split('_') {
        let index = match part.chars().next() {
            Some('P') => 0,
            Some('M') => 1,
            Some('T') => 2,
            Some('S') => 3,
            Some('D') => 4,
            _ => {
                errors.push(format!("unknown field {part}"));
                continue;
            }
        };
        if fields[index].replace(&part[1..]).is_some() {
            duplicates[index] = true;
        }
    }
    let mut values = Vec::new();
    for (index, name) in ["P", "M", "T", "S", "D"].into_iter().enumerate() {
        let parsed = if duplicates[index] {
            None
        } else {
            fields[index]
                .and_then(|raw| raw.parse::<i64>().ok())
                .and_then(|raw| match index {
                    0 => match raw {
                        0 => Some(AirConditionerValue::Power(true)),
                        1 => Some(AirConditionerValue::Power(false)),
                        _ => None,
                    },
                    1 => ac
                        .mode()
                        .read_value(&json!(raw))
                        .ok()
                        .map(AirConditionerValue::Mode),
                    2 => ac
                        .temperature()
                        .read_value(&json!(raw))
                        .ok()
                        .map(AirConditionerValue::Temperature),
                    3 => ac
                        .speed()
                        .read_value(&json!(raw))
                        .ok()
                        .map(AirConditionerValue::Speed),
                    4 => match raw {
                        0 => Some(AirConditionerValue::Swing(true)),
                        1 => Some(AirConditionerValue::Swing(false)),
                        _ => None,
                    },
                    _ => unreachable!(),
                })
        };
        match parsed {
            Some(AirConditionerValue::Swing(_)) if ac.swing().is_none() => {}
            Some(value) => values.push(value),
            None => errors.push(format!("invalid or missing {name}")),
        }
    }
    if values.is_empty() {
        return Err(CapabilityError::InvalidValue(
            "mcn04 ac-state has no valid fields",
        ));
    }
    Ok(StateReport {
        values,
        problem: (!errors.is_empty()).then(|| errors.join(", ")),
    })
}

#[derive(Deserialize)]
struct Range {
    models: Vec<RangeMode>,
}
#[derive(Deserialize)]
struct RangeMode {
    mode: i64,
    speeds: Vec<i64>,
    temps: Vec<i64>,
}

pub(super) fn range(
    ac: &AirConditionerBindings,
    value: &Value,
) -> Result<AirConditionerConstraints, CapabilityError> {
    let text = value
        .as_str()
        .ok_or(CapabilityError::InvalidValue("mcn04 ac-ctrl-range"))?;
    let input: Range = serde_json::from_str(text)
        .map_err(|_| CapabilityError::InvalidValue("mcn04 ac-ctrl-range"))?;
    let grid = ac.temperature().grid();
    let entries = input.models.into_iter().filter_map(|entry| {
        let mode = ac.mode().read_value(&json!(entry.mode)).ok()?;
        let temperatures = entry
            .temps
            .into_iter()
            .filter_map(|value| {
                i16::try_from(value)
                    .ok()
                    .filter(|value| grid.contains(*value))
            })
            .collect();
        let speeds = entry
            .speeds
            .into_iter()
            .filter_map(|value| ac.speed().read_value(&json!(value)).ok())
            .collect();
        Some((mode, temperatures, speeds))
    });
    AirConditionerConstraints::new(grid, entries)
        .map_err(|_| CapabilityError::InvalidValue("mcn04 ac-ctrl-range"))
}
