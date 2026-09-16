use super::{
    LanError, LanErrorKind,
    session::{
        LanEventArgument, LanEventArguments, LanNotification, LanProperty, LanPropertyRead,
        LanPropertyWrite, LanReadOutcome, LanWriteOutcome,
    },
};
use crate::xiaomi::{catalog::WireValue, discovery::NetworkEpoch};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;

pub(super) fn validate_property(property: &LanProperty) -> Result<(), LanError> {
    if property.siid == 0 || property.piid == 0 {
        Err(LanError::new(
            "validate LAN property",
            LanErrorKind::InvalidInput,
        ))
    } else {
        Ok(())
    }
}

pub(super) fn encode_wire(value: &WireValue) -> Result<Value, LanError> {
    match value {
        WireValue::Boolean(value) => Ok(Value::Bool(*value)),
        WireValue::Integer(value) => Ok(Value::Number((*value).into())),
        WireValue::Number(value) => Number::from_f64(*value)
            .map(Value::Number)
            .ok_or_else(|| LanError::new("encode LAN value", LanErrorKind::InvalidInput)),
        WireValue::String(value) => Ok(Value::String(value.clone())),
    }
}

pub(super) fn decode_wire(value: &Value) -> Option<WireValue> {
    match value {
        Value::Bool(value) => Some(WireValue::Boolean(*value)),
        Value::Number(value) if value.is_i64() => value.as_i64().map(WireValue::Integer),
        Value::Number(value) => value.as_f64().map(WireValue::Number),
        Value::String(value) => Some(WireValue::String(value.clone())),
        _ => None,
    }
}

pub(super) fn result_code(value: &Value) -> Option<i64> {
    value.get("result")?.get("code")?.as_i64()
}

pub(super) fn is_rpc_response(value: &Value) -> bool {
    value.get("method").is_none()
        && (value.get("result").is_some()
            || value.get("error").is_some()
            || value.get("code").is_some())
}

pub(super) fn valid_authentication_response(
    did: u64,
    property: LanProperty,
    value: &Value,
) -> bool {
    top_error(value).is_some() || parse_reads(did, &[property], value).is_ok()
}

pub(super) fn top_error(value: &Value) -> Option<i64> {
    value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
        .or_else(|| value.get("code").and_then(Value::as_i64))
}

pub(super) fn accept_result(value: &Value, operation: &'static str) -> Result<(), LanError> {
    if let Some(code) = top_error(value) {
        return Err(LanError::new(operation, LanErrorKind::Business(code)));
    }
    match result_code(value) {
        Some(0) => Ok(()),
        Some(code) => Err(LanError::new(operation, LanErrorKind::Business(code))),
        None => Err(LanError::new(operation, LanErrorKind::Protocol)),
    }
}

pub(super) fn accept_legacy(value: &Value, operation: &'static str) -> Result<(), LanError> {
    if let Some(code) = top_error(value) {
        return Err(LanError::new(operation, LanErrorKind::Business(code)));
    }
    value
        .get("result")
        .and_then(Value::as_array)
        .filter(|result| {
            !result.is_empty() && result.iter().all(|item| item.as_str() == Some("ok"))
        })
        .map(|_| ())
        .ok_or_else(|| LanError::new(operation, LanErrorKind::Protocol))
}

pub(super) fn accept_action(value: &Value, did: u64, siid: u32, aiid: u32) -> Result<(), LanError> {
    if let Some(code) = top_error(value) {
        return Err(LanError::new(
            "invoke LAN action",
            LanErrorKind::Business(code),
        ));
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| LanError::new("invoke LAN action", LanErrorKind::Protocol))?;
    match result.get("code").and_then(Value::as_i64) {
        Some(0) => {}
        Some(code) => {
            return Err(LanError::new(
                "invoke LAN action",
                LanErrorKind::Business(code),
            ));
        }
        None => return Err(LanError::new("invoke LAN action", LanErrorKind::Protocol)),
    }
    if let Some(value) = result.get("did") {
        let expected_did = did.to_string();
        let matches = value.as_str() == Some(expected_did.as_str()) || value.as_u64() == Some(did);
        if !matches {
            return Err(LanError::new("invoke LAN action", LanErrorKind::Protocol));
        }
    }
    for (key, expected) in [("siid", u64::from(siid)), ("aiid", u64::from(aiid))] {
        if result
            .get(key)
            .is_some_and(|value| value.as_u64() != Some(expected))
        {
            return Err(LanError::new("invoke LAN action", LanErrorKind::Protocol));
        }
    }
    Ok(())
}

pub(super) fn reject_duplicate_properties<'a>(
    properties: impl Iterator<Item = &'a LanProperty>,
) -> Result<(), LanError> {
    let mut seen = HashSet::new();
    if properties
        .map(|property| (property.siid, property.piid))
        .any(|key| !seen.insert(key))
    {
        return Err(LanError::new(
            "validate LAN properties",
            LanErrorKind::InvalidInput,
        ));
    }
    Ok(())
}

pub(super) fn parse_reads(
    did: u64,
    requested: &[LanProperty],
    value: &Value,
) -> Result<Vec<LanPropertyRead>, LanError> {
    if let Some(code) = top_error(value) {
        return Err(LanError::new(
            "read LAN properties",
            LanErrorKind::Business(code),
        ));
    }
    let results = value
        .get("result")
        .and_then(Value::as_array)
        .ok_or_else(|| LanError::new("read LAN properties", LanErrorKind::Protocol))?;
    if results.len() != requested.len() {
        return Err(LanError::new("read LAN properties", LanErrorKind::Protocol));
    }
    let expected_did = did.to_string();
    let mut matched = HashSet::new();
    requested
        .iter()
        .map(|property| {
            let item = results
                .iter()
                .find(|item| {
                    item.get("did").and_then(Value::as_str) == Some(expected_did.as_str())
                        && item.get("siid").and_then(Value::as_u64)
                            == Some(u64::from(property.siid))
                        && item.get("piid").and_then(Value::as_u64)
                            == Some(u64::from(property.piid))
                })
                .ok_or_else(|| LanError::new("read LAN properties", LanErrorKind::Protocol))?;
            if !matched.insert((property.siid, property.piid))
                || results
                    .iter()
                    .filter(|candidate| *candidate == item)
                    .count()
                    != 1
            {
                return Err(LanError::new("read LAN properties", LanErrorKind::Protocol));
            }
            let code = item
                .get("code")
                .and_then(Value::as_i64)
                .ok_or_else(|| LanError::new("read LAN properties", LanErrorKind::Protocol))?;
            let outcome = if code != 0 {
                LanReadOutcome::Error(code)
            } else {
                match item.get("value") {
                    None | Some(Value::Null) => LanReadOutcome::Unknown,
                    Some(value) => LanReadOutcome::Value(decode_wire(value).ok_or_else(|| {
                        LanError::new("read LAN properties", LanErrorKind::Protocol)
                    })?),
                }
            };
            Ok(LanPropertyRead {
                property: *property,
                outcome,
            })
        })
        .collect()
}

pub(super) fn parse_writes(
    did: u64,
    writes: &[LanPropertyWrite],
    value: &Value,
) -> Result<Vec<LanWriteOutcome>, LanError> {
    if let Some(code) = top_error(value) {
        return Err(LanError::new(
            "set LAN properties",
            LanErrorKind::Business(code),
        ));
    }
    let results = value
        .get("result")
        .and_then(Value::as_array)
        .ok_or_else(|| LanError::new("set LAN properties", LanErrorKind::Protocol))?;
    if results.len() != writes.len() {
        return Err(LanError::new("set LAN properties", LanErrorKind::Protocol));
    }
    let expected_did = did.to_string();
    let mut matched = HashSet::new();
    writes
        .iter()
        .map(|write| {
            let item = results
                .iter()
                .find(|item| {
                    item.get("did").and_then(Value::as_str) == Some(expected_did.as_str())
                        && item.get("siid").and_then(Value::as_u64)
                            == Some(u64::from(write.property.siid))
                        && item.get("piid").and_then(Value::as_u64)
                            == Some(u64::from(write.property.piid))
                })
                .ok_or_else(|| LanError::new("set LAN properties", LanErrorKind::Protocol))?;
            if !matched.insert((write.property.siid, write.property.piid))
                || results
                    .iter()
                    .filter(|candidate| *candidate == item)
                    .count()
                    != 1
            {
                return Err(LanError::new("set LAN properties", LanErrorKind::Protocol));
            }
            match item.get("code").and_then(Value::as_i64) {
                Some(0) => Ok(LanWriteOutcome::Accepted),
                Some(code) => Ok(LanWriteOutcome::Error(code)),
                None => Err(LanError::new("set LAN properties", LanErrorKind::Protocol)),
            }
        })
        .collect()
}

pub(super) fn parse_notifications(
    did: u64,
    epoch: NetworkEpoch,
    generation: u64,
    timestamp: u32,
    value: &Value,
) -> Result<Vec<LanNotification>, LanError> {
    let payload_did = |object: &Map<String, Value>| -> Result<(), LanError> {
        if let Some(value) = object.get("did") {
            let matches = value.as_str() == Some(&did.to_string()) || value.as_u64() == Some(did);
            if !matches {
                return Err(LanError::new(
                    "decode LAN notification",
                    LanErrorKind::Protocol,
                ));
            }
        }
        Ok(())
    };
    match value.get("method").and_then(Value::as_str) {
        Some("properties_changed") => {
            let params = value
                .get("params")
                .and_then(Value::as_array)
                .ok_or_else(|| LanError::new("decode LAN notification", LanErrorKind::Protocol))?;
            if params.is_empty() {
                return Err(LanError::new(
                    "decode LAN notification",
                    LanErrorKind::Protocol,
                ));
            }
            let mut seen = HashSet::new();
            params
                .iter()
                .map(|value| {
                    let object = value.as_object().ok_or_else(|| {
                        LanError::new("decode LAN notification", LanErrorKind::Protocol)
                    })?;
                    payload_did(object)?;
                    let siid = required_u32(object, "siid")?;
                    let piid = required_u32(object, "piid")?;
                    if !seen.insert((siid, piid)) {
                        return Err(LanError::new(
                            "decode LAN notification",
                            LanErrorKind::Protocol,
                        ));
                    }
                    Ok(LanNotification::Property {
                        did,
                        siid,
                        piid,
                        value: object.get("value").and_then(decode_wire).ok_or_else(|| {
                            LanError::new("decode LAN notification", LanErrorKind::Protocol)
                        })?,
                        epoch,
                        generation,
                        timestamp,
                    })
                })
                .collect()
        }
        Some("event_occured") => {
            let object = value
                .get("params")
                .and_then(Value::as_object)
                .ok_or_else(|| LanError::new("decode LAN notification", LanErrorKind::Protocol))?;
            payload_did(object)?;
            let arguments = parse_event_arguments(object.get("arguments"))?;
            Ok(vec![LanNotification::Event {
                did,
                siid: required_u32(object, "siid")?,
                eiid: required_u32(object, "eiid")?,
                arguments,
                epoch,
                generation,
                timestamp,
            }])
        }
        _ => Err(LanError::new(
            "decode LAN notification",
            LanErrorKind::Protocol,
        )),
    }
}

pub(super) fn parse_event_arguments(value: Option<&Value>) -> Result<LanEventArguments, LanError> {
    let values = match value {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(values)) => values,
        _ => return Err(LanError::new("decode LAN event", LanErrorKind::Protocol)),
    };
    if values.len() == 1
        && let Some(positional) = values[0].get("value").and_then(Value::as_array)
    {
        return positional
            .iter()
            .map(|value| {
                decode_wire(value)
                    .ok_or_else(|| LanError::new("decode LAN event", LanErrorKind::Protocol))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(LanEventArguments::Positional);
    }
    let mut seen = HashSet::new();
    values
        .iter()
        .map(|value| {
            let object = value
                .as_object()
                .ok_or_else(|| LanError::new("decode LAN event", LanErrorKind::Protocol))?;
            let piid = required_u32(object, "piid")?;
            if !seen.insert(piid) {
                return Err(LanError::new("decode LAN event", LanErrorKind::Protocol));
            }
            Ok(LanEventArgument {
                piid,
                value: object
                    .get("value")
                    .and_then(decode_wire)
                    .ok_or_else(|| LanError::new("decode LAN event", LanErrorKind::Protocol))?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(LanEventArguments::Keyed)
}

pub(super) fn required_u32(object: &Map<String, Value>, key: &str) -> Result<u32, LanError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| LanError::new("decode LAN notification", LanErrorKind::Protocol))
}
