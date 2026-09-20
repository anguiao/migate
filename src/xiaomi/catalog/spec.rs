use super::CompileError;
use serde_json::{Map, Value};
use std::collections::HashSet;

pub(super) struct SpecDocument {
    pub(super) type_urn: String,
    services: Vec<Value>,
}

impl SpecDocument {
    pub(super) fn parse(document: &str) -> Result<Self, CompileError> {
        let value =
            serde_json::from_str::<Value>(document).map_err(|_| CompileError::InvalidSpec)?;
        let Value::Object(mut root) = value else {
            return Err(CompileError::InvalidSpec);
        };
        let type_urn = text(&root, "type")?.to_owned();
        urn_semantic(&type_urn).ok_or(CompileError::InvalidSpec)?;
        let Some(Value::Array(services)) = root.remove("services") else {
            return Err(CompileError::InvalidSpec);
        };
        Ok(Self { type_urn, services })
    }

    pub(super) fn device_type(&self) -> &str {
        urn_semantic(&self.type_urn).expect("spec document has a validated device type")
    }

    pub(super) fn services(&self) -> Result<Vec<Service<'_>>, CompileError> {
        let services = self
            .services
            .iter()
            .map(parse_service)
            .collect::<Result<Vec<_>, _>>()?;
        validate_references(&services)?;
        Ok(services)
    }
}

#[derive(Clone)]
pub(super) struct Service<'a> {
    pub(super) iid: u32,
    pub(super) kind: &'a str,
    pub(super) name: &'a str,
    pub(super) properties: Vec<PropertySpec<'a>>,
    pub(super) actions: Vec<ActionSpec<'a>>,
    pub(super) events: Vec<ActionSpec<'a>>,
}
#[derive(Clone)]
pub(super) struct PropertySpec<'a> {
    pub(super) iid: u32,
    pub(super) kind: &'a str,
    pub(super) format: &'a str,
    pub(super) unit: Option<&'a str>,
    pub(super) access: Vec<&'a str>,
    pub(super) range: Option<(f64, f64, f64)>,
    pub(super) values: Vec<(i64, &'a str)>,
}
#[derive(Clone)]
pub(super) struct ActionSpec<'a> {
    pub(super) iid: u32,
    pub(super) kind: &'a str,
    pub(super) arguments: Vec<u32>,
}

fn parse_service(value: &Value) -> Result<Service<'_>, CompileError> {
    let object = value.as_object().ok_or(CompileError::InvalidSpec)?;
    let iid = integer(object, "iid")?;
    let kind = urn_semantic(text(object, "type")?).ok_or(CompileError::InvalidSpec)?;
    let properties = optional_array(object, "properties")?
        .iter()
        .map(parse_property)
        .collect::<Result<Vec<_>, _>>()?;
    let actions = optional_array(object, "actions")?
        .iter()
        .map(parse_action)
        .collect::<Result<Vec<_>, _>>()?;
    let events = optional_array(object, "events")?
        .iter()
        .map(parse_action)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Service {
        iid,
        kind,
        name: object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(kind),
        properties,
        actions,
        events,
    })
}
fn parse_property(value: &Value) -> Result<PropertySpec<'_>, CompileError> {
    let object = value.as_object().ok_or(CompileError::InvalidSpec)?;
    let format = text(object, "format")?;
    let values = optional_array(object, "value-list")?
        .iter()
        .map(|value| {
            let item = value.as_object().ok_or(CompileError::InvalidSpec)?;
            Ok((
                item.get("value")
                    .and_then(Value::as_i64)
                    .ok_or(CompileError::InvalidSpec)?,
                text(item, "description")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let range = match object.get("value-range") {
        None | Some(Value::Null) => None,
        Some(Value::Array(range)) => {
            if range.len() != 3 {
                return Err(CompileError::InvalidSpec);
            }
            let range = (number(&range[0])?, number(&range[1])?, number(&range[2])?);
            if range.0 > range.1 || range.2 <= 0. {
                return Err(CompileError::InvalidSpec);
            }
            Some(range)
        }
        _ => return Err(CompileError::InvalidSpec),
    };
    let access = optional_array(object, "access")?
        .iter()
        .map(|item| item.as_str().ok_or(CompileError::InvalidSpec))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PropertySpec {
        iid: integer(object, "iid")?,
        kind: urn_semantic(text(object, "type")?).ok_or(CompileError::InvalidSpec)?,
        format,
        unit: match object.get("unit") {
            None | Some(Value::Null) => None,
            Some(Value::String(unit)) => Some(unit.as_str()),
            _ => return Err(CompileError::InvalidSpec),
        },
        access,
        range,
        values,
    })
}
fn parse_action(value: &Value) -> Result<ActionSpec<'_>, CompileError> {
    let object = value.as_object().ok_or(CompileError::InvalidSpec)?;
    let values = object.get("arguments").or_else(|| object.get("in"));
    let arguments = match values {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(CompileError::InvalidSpec)
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(CompileError::InvalidSpec),
    };
    Ok(ActionSpec {
        iid: integer(object, "iid")?,
        kind: urn_semantic(text(object, "type")?).ok_or(CompileError::InvalidSpec)?,
        arguments,
    })
}

fn validate_references(services: &[Service<'_>]) -> Result<(), CompileError> {
    if !unique(services.iter().map(|service| service.iid)) {
        return Err(CompileError::InvalidSpec);
    }
    for service in services {
        if !unique(service.properties.iter().map(|property| property.iid))
            || !unique(service.actions.iter().map(|action| action.iid))
            || !unique(service.events.iter().map(|event| event.iid))
        {
            return Err(CompileError::InvalidSpec);
        }
        for operation in service.actions.iter().chain(&service.events) {
            if operation.arguments.iter().any(|iid| {
                !service
                    .properties
                    .iter()
                    .any(|property| property.iid == *iid)
            }) {
                return Err(CompileError::InvalidSpec);
            }
        }
    }
    Ok(())
}

pub(super) fn urn_semantic(value: &str) -> Option<&str> {
    value.split(':').nth(3).filter(|value| !value.is_empty())
}
fn optional_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a [Value], CompileError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(CompileError::InvalidSpec),
    }
}
fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, CompileError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(CompileError::InvalidSpec)
}
fn integer(object: &Map<String, Value>, key: &str) -> Result<u32, CompileError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|value| *value > 0)
        .ok_or(CompileError::InvalidSpec)
}
fn number(value: &Value) -> Result<f64, CompileError> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or(CompileError::InvalidSpec)
}

fn unique(values: impl IntoIterator<Item = u32>) -> bool {
    let mut seen = HashSet::new();
    values.into_iter().all(|value| seen.insert(value))
}

pub(super) fn numeric_format(format: &str) -> bool {
    matches!(
        format,
        "float" | "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64"
    )
}
