use std::collections::HashSet;

use serde_json::Value;

use super::{CapabilityError, NumericFormat, PropertyBinding};

#[derive(Clone, Copy)]
pub(super) struct Urn<'a> {
    pub(super) namespace: &'a str,
    pub(super) entity: &'a str,
    pub(super) name: &'a str,
    pub(super) type_id: u32,
    pub(super) vendor: &'a str,
}

impl<'a> Urn<'a> {
    pub(super) fn parse(value: &'a str) -> Result<Self, CapabilityError> {
        let fields = value.split(':').collect::<Vec<_>>();
        let [
            prefix,
            namespace,
            entity,
            name,
            type_id,
            vendor,
            version,
            tail @ ..,
        ] = fields.as_slice()
        else {
            return Err(CapabilityError::InvalidDescription("type"));
        };
        match tail {
            [] => {}
            [suffix] if hexadecimal_id(suffix) => {}
            _ => return Err(CapabilityError::InvalidDescription("type")),
        }
        if *prefix != "urn"
            || !token(namespace)
            || !matches!(
                *entity,
                "device" | "service" | "property" | "action" | "event"
            )
            || !token(name)
            || !hexadecimal_id(type_id)
            || !token(vendor)
            || !version.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(CapabilityError::InvalidDescription("type"));
        }
        version
            .parse::<u32>()
            .ok()
            .filter(|version| *version > 0)
            .ok_or(CapabilityError::InvalidDescription("type"))?;
        Ok(Self {
            namespace,
            entity,
            name,
            type_id: u32::from_str_radix(type_id, 16)
                .map_err(|_| CapabilityError::InvalidDescription("type"))?,
            vendor,
        })
    }

    pub(super) fn matches(&self, namespace: &str, entity: &str, name: &str, type_id: u32) -> bool {
        self.namespace == namespace
            && self.entity == entity
            && self.name == name
            && self.type_id == type_id
    }

    pub(super) fn standard(&self, entity: &str, name: &str, type_id: u32) -> bool {
        self.matches("miot-spec-v2", entity, name, type_id)
    }
}

fn hexadecimal_id(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

pub(super) struct Description<'a> {
    pub(super) urn: Urn<'a>,
    pub(super) services: Vec<Service<'a>>,
}

pub(super) struct Service<'a> {
    pub(super) iid: u32,
    pub(super) urn: Urn<'a>,
    pub(super) properties: Vec<Property<'a>>,
    pub(super) actions: Vec<Action<'a>>,
    pub(super) events: Vec<Event<'a>>,
}

pub(super) struct Action<'a> {
    pub(super) iid: u32,
    pub(super) urn: Urn<'a>,
    pub(super) value: &'a Value,
}

pub(super) struct Event<'a> {
    pub(super) iid: u32,
    pub(super) urn: Urn<'a>,
    pub(super) arguments: Vec<u32>,
}

pub(super) struct Property<'a> {
    pub(super) iid: u32,
    pub(super) urn: Urn<'a>,
    pub(super) value: &'a Value,
}

impl<'a> Description<'a> {
    pub(super) fn parse(value: &'a Value) -> Result<Self, CapabilityError> {
        let urn = typed_urn(value, "device")?;
        let mut service_ids = HashSet::new();
        let mut services = Vec::new();
        for service in array(value, "services")? {
            let urn = typed_urn(service, "service")?;
            let iid = instance_id(service)?;
            if !service_ids.insert(iid) {
                return Err(CapabilityError::InvalidDescription("duplicate service iid"));
            }
            let mut property_ids = HashSet::new();
            let mut properties = Vec::new();
            if service.get("properties").is_some() {
                for property in array(service, "properties")? {
                    let urn = typed_urn(property, "property")?;
                    let iid = instance_id(property)?;
                    if !property_ids.insert(iid) {
                        return Err(CapabilityError::InvalidDescription(
                            "duplicate property iid",
                        ));
                    }
                    properties.push(Property {
                        iid,
                        urn,
                        value: property,
                    });
                }
            }
            let mut actions = Vec::new();
            let mut action_ids = HashSet::new();
            if service.get("actions").is_some() {
                for action in array(service, "actions")? {
                    let urn = typed_urn(action, "action")?;
                    let iid = instance_id(action)?;
                    if !action_ids.insert(iid) {
                        return Err(CapabilityError::InvalidDescription("duplicate action iid"));
                    }
                    // Argument shapes are only interpreted by the model patch
                    // that consumes a specific action.
                    actions.push(Action {
                        iid,
                        urn,
                        value: action,
                    });
                }
            }
            let mut events = Vec::new();
            let mut event_ids = HashSet::new();
            if service.get("events").is_some() {
                for event in array(service, "events")? {
                    let urn = typed_urn(event, "event")?;
                    let iid = instance_id(event)?;
                    if !event_ids.insert(iid) {
                        return Err(CapabilityError::InvalidDescription("duplicate event iid"));
                    }
                    let arguments = array(event, "arguments")?
                        .iter()
                        .map(|argument| {
                            argument
                                .as_u64()
                                .and_then(|id| u32::try_from(id).ok())
                                .filter(|id| property_ids.contains(id))
                                .ok_or(CapabilityError::InvalidDescription("event arguments"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    events.push(Event {
                        iid,
                        urn,
                        arguments,
                    });
                }
            }
            services.push(Service {
                iid,
                urn,
                properties,
                actions,
                events,
            });
        }
        Ok(Self { urn, services })
    }
}

fn typed_urn<'a>(value: &'a Value, entity: &str) -> Result<Urn<'a>, CapabilityError> {
    let urn = Urn::parse(string(value, "type")?)?;
    if urn.entity != entity {
        return Err(CapabilityError::InvalidDescription("type entity"));
    }
    Ok(urn)
}

pub(super) fn string<'a>(
    value: &'a Value,
    field: &'static str,
) -> Result<&'a str, CapabilityError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CapabilityError::InvalidDescription(field))
}

pub(super) fn array<'a>(
    value: &'a Value,
    field: &'static str,
) -> Result<&'a [Value], CapabilityError> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(CapabilityError::InvalidDescription(field))
}

pub(super) fn binding(service: &Service<'_>, property: &Property<'_>) -> PropertyBinding {
    PropertyBinding {
        siid: service.iid,
        piid: property.iid,
    }
}

impl<'a> Service<'a> {
    pub(super) fn property(
        &self,
        name: &'static str,
        type_id: u32,
    ) -> Result<&Property<'a>, CapabilityError> {
        self.optional_property(name, type_id)?
            .ok_or(CapabilityError::MissingFunction(name))
    }

    pub(super) fn optional_property(
        &self,
        name: &str,
        type_id: u32,
    ) -> Result<Option<&Property<'a>>, CapabilityError> {
        self.matching_property(|property| property.urn.standard("property", name, type_id))
    }

    pub(super) fn matching_property(
        &self,
        matches: impl Fn(&Property<'_>) -> bool,
    ) -> Result<Option<&Property<'a>>, CapabilityError> {
        let mut properties = self.properties.iter().filter(|property| matches(property));
        let first = properties.next();
        if properties.next().is_some() {
            return Err(CapabilityError::InvalidDescription("ambiguous property"));
        }
        Ok(first)
    }
}

pub(super) struct Access<'a>(&'a [Value]);

impl Access<'_> {
    pub(super) fn contains(&self, permission: &str) -> bool {
        self.0.iter().any(|item| item.as_str() == Some(permission))
    }
}

impl<'a> Property<'a> {
    pub(super) fn access(&self, required: &[&'static str]) -> Result<Access<'a>, CapabilityError> {
        let values = array(self.value, "access")?;
        if values.iter().any(|item| item.as_str().is_none()) {
            return Err(CapabilityError::InvalidDescription("access"));
        }
        let access = Access(values);
        for &permission in required {
            if !access.contains(permission) {
                return Err(CapabilityError::MissingAccess(permission));
            }
        }
        Ok(access)
    }

    pub(super) fn control_access(&self) -> Result<(), CapabilityError> {
        self.access(&["read", "write", "notify"])?;
        Ok(())
    }

    pub(super) fn boolean_control(&self) -> Result<(), CapabilityError> {
        self.control_access()?;
        if string(self.value, "format")? != "bool" {
            return Err(CapabilityError::InvalidDescription(
                "boolean property format",
            ));
        }
        Ok(())
    }

    pub(super) fn integer_format(&self) -> Result<NumericFormat, CapabilityError> {
        match string(self.value, "format")? {
            "uint8" => Ok(NumericFormat::Uint8),
            "uint16" => Ok(NumericFormat::Uint16),
            "uint32" => Ok(NumericFormat::Uint32),
            "int8" => Ok(NumericFormat::Int8),
            "int16" => Ok(NumericFormat::Int16),
            "int32" => Ok(NumericFormat::Int32),
            _ => Err(CapabilityError::InvalidDescription(
                "integer property format",
            )),
        }
    }

    pub(super) fn numeric_format(&self) -> Result<NumericFormat, CapabilityError> {
        if string(self.value, "format")? == "float" {
            Ok(NumericFormat::Float)
        } else {
            self.integer_format()
        }
    }
}

fn instance_id(value: &Value) -> Result<u32, CapabilityError> {
    value
        .get("iid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(CapabilityError::InvalidDescription("iid"))
}
