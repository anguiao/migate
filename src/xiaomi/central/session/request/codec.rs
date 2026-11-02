use super::{CentralError, DeviceAccess, DeviceEntry, RequestKind, Response, ResultValue};
use crate::xiaomi::{
    capability::PropertyBinding,
    property::{
        PropertyReading, correlate, update_time, valid_bindings, valid_observation, valid_value,
    },
};
use serde_json::{Value, json};
use std::sync::{
    OnceLock,
    atomic::{AtomicU32, Ordering},
};

pub(in crate::xiaomi::central::session) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
pub(super) fn valid_request(kind: &RequestKind) -> bool {
    let (did, binding) = match kind {
        RequestKind::ListAll => return true,
        RequestKind::ListFiltered { dids } => {
            return !dids.is_empty() && dids.iter().all(|did| valid_identifier(did));
        }
        RequestKind::Unsubscribe { did, .. } => (did, None),
        RequestKind::Subscribe { did, binding } => {
            return valid_identifier(did) && binding.valid();
        }
        RequestKind::WriteProperties { did, properties } => {
            return valid_identifier(did)
                && valid_bindings(
                    &properties
                        .iter()
                        .map(|(binding, _)| *binding)
                        .collect::<Vec<_>>(),
                )
                && properties.iter().all(|(_, value)| valid_value(value));
        }
        RequestKind::Read { did, binding } | RequestKind::Write { did, binding, .. } => {
            (did, Some(binding))
        }
    };
    valid_identifier(did) && binding.is_none_or(|binding| binding.siid > 0 && binding.piid > 0)
}

pub(super) fn write_payload(
    did: &str,
    properties: &[(PropertyBinding, Value)],
) -> (&'static str, Value) {
    let params: Vec<_> = properties.iter().map(|(binding, value)| {
        json!({"did":did,"siid":binding.siid,"piid":binding.piid,"value":value})
    }).collect();
    (
        "master/proxy/rpcReq",
        json!({"did":did,"rpc":{"id":next_mips_id(),"method":"set_properties","params":params}}),
    )
}

pub(in crate::xiaomi::central::session) fn next_mips_id() -> u32 {
    // MIPS uses a wrapping 32-bit sequence. Keep it across connections so a
    // reconnect cannot immediately reuse an outstanding request's identifier.
    static IDS: OnceLock<AtomicU32> = OnceLock::new();
    IDS.get_or_init(|| AtomicU32::new(rand::random()))
        .fetch_add(1, Ordering::Relaxed)
}
pub(in crate::xiaomi::central::session) fn validate_identity(
    value: &Value,
    did: &str,
    binding: PropertyBinding,
    required: bool,
) -> Result<(), CentralError> {
    for (name, expected) in [("siid", binding.siid), ("piid", binding.piid)] {
        if (required || value.get(name).is_some())
            && value.get(name).and_then(Value::as_u64) != Some(u64::from(expected))
        {
            return Err(CentralError::Protocol);
        }
    }
    if (required || value.get("did").is_some())
        && value.get("did").and_then(Value::as_str) != Some(did)
    {
        return Err(CentralError::Protocol);
    }
    Ok(())
}
pub(in crate::xiaomi::central::session) fn parse_response(
    kind: &RequestKind,
    value: &Value,
) -> ResultValue {
    if let Some(error) = value.get("error") {
        return Err(CentralError::Rejected(
            error
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?,
        ));
    }
    match kind {
        RequestKind::Read { did, binding } => {
            validate_identity(value, did, *binding, false)?;
            Ok(Response::PropertyValue(PropertyReading {
                value: value
                    .get("value")
                    .filter(|value| valid_observation(value))
                    .cloned()
                    .ok_or(CentralError::Protocol)?,
                update_time: update_time(value),
            }))
        }
        RequestKind::WriteProperties { did, properties } => {
            let result = value
                .get("result")
                .and_then(Value::as_array)
                .ok_or(CentralError::Protocol)?;
            let bindings: Vec<_> = properties.iter().map(|(binding, _)| *binding).collect();
            Ok(Response::Properties(correlate(
                did,
                &bindings,
                result,
                CentralError::Protocol,
                |item| match item
                    .get("code")
                    .and_then(Value::as_i64)
                    .ok_or(CentralError::Protocol)?
                {
                    0 | 1 => Ok(()),
                    code => Err(CentralError::Rejected(code)),
                },
            )))
        }
        RequestKind::Write { did, binding, .. } => {
            let result = value
                .get("result")
                .and_then(Value::as_array)
                .filter(|result| result.len() == 1)
                .ok_or(CentralError::Protocol)?;
            let result = &result[0];
            if result.get("did").and_then(Value::as_str) != Some(did) {
                return Err(CentralError::Protocol);
            }
            validate_identity(result, did, *binding, false)?;
            match result
                .get("code")
                .and_then(Value::as_i64)
                .ok_or(CentralError::Protocol)?
            {
                0 | 1 => Ok(Response::Unit),
                code => Err(CentralError::Rejected(code)),
            }
        }
        RequestKind::ListAll | RequestKind::ListFiltered { .. } => {
            let list = value
                .get("devList")
                .and_then(Value::as_object)
                .ok_or(CentralError::Protocol)?;
            let mut entries = Vec::new();
            for (did, info) in list {
                if let RequestKind::ListFiltered { dids } = kind
                    && !dids.contains(did)
                {
                    continue;
                }
                if !valid_identifier(did) {
                    return Err(CentralError::Protocol);
                }
                entries.push(DeviceEntry {
                    did: did.clone(),
                    access: device_access(did, info),
                });
            }
            Ok(Response::Devices(entries))
        }
        RequestKind::Subscribe { .. } | RequestKind::Unsubscribe { .. } => {
            Err(CentralError::Protocol)
        }
    }
}

fn device_access(did: &str, info: &Value) -> Result<DeviceAccess, CentralError> {
    let text = |key| {
        info.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or(CentralError::Protocol)
    };
    let flag = |key| {
        info.get(key)
            .and_then(Value::as_bool)
            .ok_or(CentralError::Protocol)
    };
    let name = match info.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name))
            if name.trim().is_empty() || name.chars().any(char::is_control) =>
        {
            None
        }
        Some(Value::String(name)) => Some(name.clone()),
        _ => return Err(CentralError::Protocol),
    };
    Ok(DeviceAccess {
        did: did.into(),
        name,
        model: text("model")?,
        instance_type: text("urn")?,
        online: flag("online")?,
        spec_v2_access: flag("specV2Access")?,
        push_available: flag("pushAvailable")?,
    })
}
