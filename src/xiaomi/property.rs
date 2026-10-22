use super::capability::PropertyBinding;
use serde_json::Value;

/// One decoded real property value and any upstream attribute update time.
/// The time is preserved exactly as supplied; it is diagnostic only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyReading {
    pub value: Value,
    pub update_time: Option<i64>,
}

/// A supplied upstream attribute update time from a read result or
/// notification. Only the explicit attribute field is accepted; message
/// timestamps and transport completion time are never substituted here.
pub(crate) fn update_time(value: &Value) -> Option<i64> {
    value.as_object().and_then(update_time_map)
}

pub(crate) fn update_time_map(map: &serde_json::Map<String, Value>) -> Option<i64> {
    map.get("updateTime").and_then(number)
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
}

/// Results remain in request order, including confirmed items from an invalid batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyResults<T, E> {
    pub items: Vec<Result<T, E>>,
    /// Missing, duplicate, malformed or unsolicited results invalidate the batch.
    pub protocol_error: bool,
}

pub(crate) fn valid_value(value: &Value) -> bool {
    value.is_boolean() || value.as_f64().is_some_and(f64::is_finite)
}

pub(crate) fn valid_bindings(bindings: &[PropertyBinding]) -> bool {
    !bindings.is_empty()
        && bindings.iter().enumerate().all(|(index, binding)| {
            binding.siid > 0 && binding.piid > 0 && !bindings[..index].contains(binding)
        })
}

pub(crate) fn correlate<T, E: Clone + PartialEq>(
    did: &str,
    bindings: &[PropertyBinding],
    values: &[Value],
    protocol: E,
    decode: impl Fn(&Value) -> Result<T, E>,
) -> PropertyResults<T, E> {
    let mut items: Vec<Option<Result<T, E>>> = (0..bindings.len()).map(|_| None).collect();
    let mut protocol_error = false;
    for value in values {
        let index = bindings.iter().position(|binding| {
            value.get("did").and_then(Value::as_str) == Some(did)
                && value.get("siid").and_then(Value::as_u64) == Some(u64::from(binding.siid))
                && value.get("piid").and_then(Value::as_u64) == Some(u64::from(binding.piid))
        });
        let Some(index) = index else {
            protocol_error = true;
            continue;
        };
        let result = if items[index].is_some() {
            Err(protocol.clone())
        } else {
            decode(value)
        };
        protocol_error |= result.as_ref().err() == Some(&protocol);
        items[index] = Some(result);
    }
    let items = items
        .into_iter()
        .map(|item| match item {
            Some(result) => result,
            None => {
                protocol_error = true;
                Err(protocol.clone())
            }
        })
        .collect();
    PropertyResults {
        items,
        protocol_error,
    }
}
