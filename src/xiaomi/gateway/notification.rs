use crate::xiaomi::catalog::WireValue;
use serde_json::{Map, Value};

use super::MipsError;

#[derive(Clone, Debug, PartialEq)]
pub enum EventArguments {
    Keyed(Vec<(u32, WireValue)>),
    Positional(Vec<WireValue>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum GatewayNotification {
    DeviceListChanged {
        dids: Vec<String>,
        epoch: u64,
        generation: u64,
    },
    Property {
        did: String,
        siid: u32,
        piid: u32,
        value: Option<WireValue>,
        epoch: u64,
        generation: u64,
    },
    Event {
        did: String,
        siid: u32,
        eiid: u32,
        arguments: EventArguments,
        epoch: u64,
        generation: u64,
    },
}

impl GatewayNotification {
    pub fn parse(
        topic: &str,
        payload: &[u8],
        retained: bool,
        selected_dids: &[String],
        epoch: u64,
        generation: u64,
    ) -> Result<Self, MipsError> {
        if retained {
            return Err(MipsError);
        }
        let segments = topic.split('/').collect::<Vec<_>>();
        if segments.len() == 3 && segments[1..] == ["appMsg", "devListChange"] {
            let envelope = super::MipsEnvelope::decode(payload)?;
            let value = serde_json::from_str::<Value>(&envelope.payload).map_err(|_| MipsError)?;
            let values = value
                .get("devList")
                .and_then(Value::as_array)
                .filter(|values| !values.is_empty())
                .ok_or(MipsError)?;
            let mut dids = Vec::with_capacity(values.len());
            for value in values {
                let did = value
                    .as_str()
                    .filter(|did| {
                        !did.is_empty()
                            && !did.contains(['/', '+', '#', '\0'])
                            && !did.chars().any(char::is_control)
                    })
                    .ok_or(MipsError)?;
                if dids.iter().any(|existing| existing == did) {
                    return Err(MipsError);
                }
                dids.push(did.to_owned());
            }
            return Ok(Self::DeviceListChanged {
                dids,
                epoch,
                generation,
            });
        }
        if segments.len() != 7
            || segments[1..4] != ["appMsg", "notify", "iot"]
            || !selected_dids.iter().any(|did| did == segments[4])
        {
            return Err(MipsError);
        }
        let (topic_siid, topic_iid) = parse_local_suffix(segments[6])?;
        let envelope = super::MipsEnvelope::decode(payload)?;
        let object = serde_json::from_str::<Value>(&envelope.payload)
            .map_err(|_| MipsError)?
            .as_object()
            .cloned()
            .ok_or(MipsError)?;
        if string(&object, "did")? != segments[4] || integer(&object, "siid")? != topic_siid {
            return Err(MipsError);
        }
        match segments[5] {
            "property" if integer(&object, "piid")? == topic_iid => Ok(Self::Property {
                did: segments[4].to_owned(),
                siid: topic_siid,
                piid: topic_iid,
                value: object.get("value").and_then(wire_value),
                epoch,
                generation,
            }),
            "event" if integer(&object, "eiid")? == topic_iid => Ok(Self::Event {
                did: segments[4].to_owned(),
                siid: topic_siid,
                eiid: topic_iid,
                arguments: object
                    .get("arguments")
                    .map(parse_arguments)
                    .transpose()?
                    .unwrap_or_else(|| EventArguments::Positional(Vec::new())),
                epoch,
                generation,
            }),
            _ => Err(MipsError),
        }
    }

    pub fn value(&self) -> Option<&WireValue> {
        match self {
            Self::Property { value, .. } => value.as_ref(),
            Self::Event { .. } | Self::DeviceListChanged { .. } => None,
        }
    }

    pub fn arguments(&self) -> Option<&EventArguments> {
        match self {
            Self::Event { arguments, .. } => Some(arguments),
            Self::Property { .. } | Self::DeviceListChanged { .. } => None,
        }
    }

    pub fn epoch(&self) -> u64 {
        match self {
            Self::Property { epoch, .. }
            | Self::Event { epoch, .. }
            | Self::DeviceListChanged { epoch, .. } => *epoch,
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Property { generation, .. }
            | Self::Event { generation, .. }
            | Self::DeviceListChanged { generation, .. } => *generation,
        }
    }
}

pub(crate) fn wire_value(value: &Value) -> Option<WireValue> {
    match value {
        Value::Bool(value) => Some(WireValue::Boolean(*value)),
        Value::Number(value) if value.is_i64() => value.as_i64().map(WireValue::Integer),
        Value::Number(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(WireValue::Number),
        Value::String(value) => Some(WireValue::String(value.clone())),
        _ => None,
    }
}

fn parse_arguments(value: &Value) -> Result<EventArguments, MipsError> {
    let values = value.as_array().ok_or(MipsError)?;
    if values.iter().all(Value::is_object) {
        let mut keyed = Vec::with_capacity(values.len());
        for value in values {
            let object = value.as_object().ok_or(MipsError)?;
            let piid = integer(object, "piid")?;
            let value = object.get("value").and_then(wire_value).ok_or(MipsError)?;
            if keyed.iter().any(|(existing, _)| *existing == piid) {
                return Err(MipsError);
            }
            keyed.push((piid, value));
        }
        return Ok(EventArguments::Keyed(keyed));
    }
    let positional = values
        .iter()
        .map(|value| wire_value(value).ok_or(MipsError))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EventArguments::Positional(positional))
}

fn parse_local_suffix(value: &str) -> Result<(u32, u32), MipsError> {
    let (siid, iid) = value.split_once('.').ok_or(MipsError)?;
    Ok((
        siid.parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(MipsError)?,
        iid.parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(MipsError)?,
    ))
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, MipsError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(MipsError)
}

fn integer(object: &Map<String, Value>, key: &str) -> Result<u32, MipsError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(MipsError)
}
