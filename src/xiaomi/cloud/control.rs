use super::{CloudClient, CloudError, CloudErrorKind};
use crate::xiaomi::catalog::WireValue;
use serde_json::{Map, Number, Value, json};
use std::{collections::HashMap, time::Instant};

const READ_PROPERTIES: &str = "read Xiaomi properties";
const SET_PROPERTIES: &str = "set Xiaomi properties";
const INVOKE_ACTION: &str = "invoke Xiaomi action";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CloudProperty {
    did: String,
    siid: u32,
    piid: u32,
}

impl CloudProperty {
    pub fn new(did: impl Into<String>, siid: u32, piid: u32) -> Self {
        Self {
            did: did.into(),
            siid,
            piid,
        }
    }

    pub fn did(&self) -> &str {
        &self.did
    }

    pub fn siid(&self) -> u32 {
        self.siid
    }

    pub fn piid(&self) -> u32 {
        self.piid
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PropertyReadOutcome {
    Value(WireValue),
    Unknown,
    Error(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropertyRead {
    property: CloudProperty,
    outcome: PropertyReadOutcome,
}

impl PropertyRead {
    pub fn new(property: CloudProperty, outcome: PropertyReadOutcome) -> Self {
        Self { property, outcome }
    }

    pub fn property(&self) -> &CloudProperty {
        &self.property
    }

    pub fn outcome(&self) -> &PropertyReadOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropertyWrite {
    property: CloudProperty,
    value: WireValue,
}

impl PropertyWrite {
    pub fn new(property: CloudProperty, value: WireValue) -> Self {
        Self { property, value }
    }

    pub fn property(&self) -> &CloudProperty {
        &self.property
    }

    pub fn value(&self) -> &WireValue {
        &self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PropertyWriteOutcome {
    Accepted,
    Error(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyWriteResult {
    property: CloudProperty,
    outcome: PropertyWriteOutcome,
}

impl PropertyWriteResult {
    pub fn new(property: CloudProperty, outcome: PropertyWriteOutcome) -> Self {
        Self { property, outcome }
    }

    pub fn property(&self) -> &CloudProperty {
        &self.property
    }

    pub fn outcome(&self) -> &PropertyWriteOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CloudAction {
    did: String,
    siid: u32,
    aiid: u32,
    input: Vec<WireValue>,
}

impl CloudAction {
    pub fn new(did: impl Into<String>, siid: u32, aiid: u32, input: Vec<WireValue>) -> Self {
        Self {
            did: did.into(),
            siid,
            aiid,
            input,
        }
    }

    pub fn did(&self) -> &str {
        &self.did
    }

    pub fn siid(&self) -> u32 {
        self.siid
    }

    pub fn aiid(&self) -> u32 {
        self.aiid
    }

    pub fn input(&self) -> &[WireValue] {
        &self.input
    }
}

impl CloudClient {
    pub async fn read_properties(
        &self,
        access_token: &str,
        properties: &[CloudProperty],
        deadline: Instant,
    ) -> Result<Vec<PropertyRead>, CloudError> {
        validate_properties(properties, READ_PROPERTIES)?;
        let timeout = self.control_request_timeout(deadline, READ_PROPERTIES)?;
        let params = properties.iter().map(property_json).collect::<Vec<_>>();
        let value = self
            .protected_post_with_timeout(
                READ_PROPERTIES,
                "/app/v2/miotspec/prop/get",
                access_token,
                json!({"datasource": 1, "params": params}),
                Some(timeout),
            )
            .await?;
        let results = result_array(&value, READ_PROPERTIES)?;
        let mut parsed = parse_property_results(results, properties, READ_PROPERTIES)?;
        properties
            .iter()
            .map(|property| {
                let item = parsed
                    .remove(property)
                    .ok_or_else(|| CloudError::protocol(READ_PROPERTIES))?;
                let code = result_code(&item, READ_PROPERTIES)?;
                let outcome = if code != 0 {
                    PropertyReadOutcome::Error(code)
                } else {
                    item.get("value")
                        .map(wire_value)
                        .unwrap_or(PropertyReadOutcome::Unknown)
                };
                Ok(PropertyRead::new(property.clone(), outcome))
            })
            .collect()
    }

    pub async fn set_properties(
        &self,
        access_token: &str,
        writes: &[PropertyWrite],
        deadline: Instant,
    ) -> Result<Vec<PropertyWriteResult>, CloudError> {
        if writes.is_empty() {
            return Err(CloudError::input(SET_PROPERTIES));
        }
        let properties = writes
            .iter()
            .map(|write| write.property.clone())
            .collect::<Vec<_>>();
        validate_properties(&properties, SET_PROPERTIES)?;
        let params = writes
            .iter()
            .map(|write| {
                let mut value = property_json(&write.property);
                value["value"] = encode_wire_value(&write.value, SET_PROPERTIES)?;
                Ok(value)
            })
            .collect::<Result<Vec<_>, CloudError>>()?;
        let timeout = self.control_request_timeout(deadline, SET_PROPERTIES)?;
        let value = self
            .protected_post_with_timeout(
                SET_PROPERTIES,
                "/app/v2/miotspec/prop/set",
                access_token,
                json!({"params": params}),
                Some(timeout),
            )
            .await?;
        let results = result_array(&value, SET_PROPERTIES)?;
        let mut parsed = parse_property_results(results, &properties, SET_PROPERTIES)?;
        properties
            .into_iter()
            .map(|property| {
                let item = parsed
                    .remove(&property)
                    .ok_or_else(|| CloudError::protocol(SET_PROPERTIES))?;
                let code = result_code(&item, SET_PROPERTIES)?;
                let outcome = if code == 0 {
                    PropertyWriteOutcome::Accepted
                } else {
                    PropertyWriteOutcome::Error(code)
                };
                Ok(PropertyWriteResult::new(property, outcome))
            })
            .collect()
    }

    pub async fn invoke_action(
        &self,
        access_token: &str,
        action: &CloudAction,
        deadline: Instant,
    ) -> Result<(), CloudError> {
        validate_action(action)?;
        let input = action
            .input
            .iter()
            .map(|value| encode_wire_value(value, INVOKE_ACTION))
            .collect::<Result<Vec<_>, _>>()?;
        let timeout = self.control_request_timeout(deadline, INVOKE_ACTION)?;
        let value = self
            .protected_post_with_timeout(
                INVOKE_ACTION,
                "/app/v2/miotspec/action",
                access_token,
                json!({"params": {
                    "did": action.did,
                    "siid": action.siid,
                    "aiid": action.aiid,
                    "in": input,
                }}),
                Some(timeout),
            )
            .await?;
        let result = result_object(&value, INVOKE_ACTION)?;
        if string_field(result, "did", INVOKE_ACTION)? != action.did
            || u32_field(result, "siid", INVOKE_ACTION)? != action.siid
            || u32_field(result, "aiid", INVOKE_ACTION)? != action.aiid
        {
            return Err(CloudError::protocol(INVOKE_ACTION));
        }
        let code = result_code(result, INVOKE_ACTION)?;
        if code != 0 {
            return Err(CloudError::new(
                INVOKE_ACTION,
                CloudErrorKind::Business(code),
            ));
        }
        Ok(())
    }

    fn control_request_timeout(
        &self,
        deadline: Instant,
        operation: &'static str,
    ) -> Result<std::time::Duration, CloudError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CloudError::new(operation, CloudErrorKind::Timeout));
        }
        Ok(remaining.min(self.control_timeout))
    }
}

fn validate_properties(
    properties: &[CloudProperty],
    operation: &'static str,
) -> Result<(), CloudError> {
    if properties.is_empty()
        || properties
            .iter()
            .any(|property| property.did.is_empty() || property.siid == 0 || property.piid == 0)
        || properties
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != properties.len()
    {
        return Err(CloudError::input(operation));
    }
    Ok(())
}

fn validate_action(action: &CloudAction) -> Result<(), CloudError> {
    if action.did.is_empty() || action.siid == 0 || action.aiid == 0 {
        return Err(CloudError::input(INVOKE_ACTION));
    }
    Ok(())
}

fn property_json(property: &CloudProperty) -> Value {
    json!({"did": property.did, "siid": property.siid, "piid": property.piid})
}

fn encode_wire_value(value: &WireValue, operation: &'static str) -> Result<Value, CloudError> {
    match value {
        WireValue::Boolean(value) => Ok(Value::Bool(*value)),
        WireValue::Integer(value) => Ok(Value::Number((*value).into())),
        WireValue::Number(value) => Number::from_f64(*value)
            .map(Value::Number)
            .ok_or_else(|| CloudError::input(operation)),
        WireValue::String(value) => Ok(Value::String(value.clone())),
    }
}

fn wire_value(value: &Value) -> PropertyReadOutcome {
    match value {
        Value::Bool(value) => PropertyReadOutcome::Value(WireValue::Boolean(*value)),
        Value::Number(value) if value.is_i64() => {
            PropertyReadOutcome::Value(WireValue::Integer(value.as_i64().unwrap()))
        }
        Value::Number(value) if value.is_f64() => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(WireValue::Number)
            .map(PropertyReadOutcome::Value)
            .unwrap_or(PropertyReadOutcome::Unknown),
        Value::String(value) => PropertyReadOutcome::Value(WireValue::String(value.clone())),
        _ => PropertyReadOutcome::Unknown,
    }
}

fn result_array<'a>(value: &'a Value, operation: &'static str) -> Result<&'a [Value], CloudError> {
    envelope_result(value, operation)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn result_object<'a>(
    value: &'a Value,
    operation: &'static str,
) -> Result<&'a Map<String, Value>, CloudError> {
    envelope_result(value, operation)?
        .as_object()
        .ok_or_else(|| CloudError::protocol(operation))
}

fn envelope_result<'a>(value: &'a Value, operation: &'static str) -> Result<&'a Value, CloudError> {
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| CloudError::protocol(operation))?;
    if code != 0 {
        return Err(CloudError::new(operation, CloudErrorKind::Business(code)));
    }
    value
        .get("result")
        .ok_or_else(|| CloudError::protocol(operation))
}

fn parse_property_results(
    results: &[Value],
    expected: &[CloudProperty],
    operation: &'static str,
) -> Result<HashMap<CloudProperty, Map<String, Value>>, CloudError> {
    if results.len() != expected.len() {
        return Err(CloudError::protocol(operation));
    }
    let expected = expected.iter().collect::<std::collections::HashSet<_>>();
    let mut parsed = HashMap::new();
    for value in results {
        let item = value
            .as_object()
            .ok_or_else(|| CloudError::protocol(operation))?;
        let property = CloudProperty::new(
            string_field(item, "did", operation)?,
            u32_field(item, "siid", operation)?,
            u32_field(item, "piid", operation)?,
        );
        if !expected.contains(&property) || parsed.insert(property, item.clone()).is_some() {
            return Err(CloudError::protocol(operation));
        }
    }
    Ok(parsed)
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    operation: &'static str,
) -> Result<&'a str, CloudError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CloudError::protocol(operation))
}

fn u32_field(
    object: &Map<String, Value>,
    field: &str,
    operation: &'static str,
) -> Result<u32, CloudError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn result_code(object: &Map<String, Value>, operation: &'static str) -> Result<i64, CloudError> {
    object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| CloudError::protocol(operation))
}
