use std::collections::HashSet;

use serde_json::Value;

use super::CapabilityError;

#[derive(Clone, Copy)]
pub(super) struct Urn<'a> {
    pub namespace: &'a str,
    pub entity: &'a str,
    pub name: &'a str,
    pub type_id: u32,
    pub vendor: &'a str,
}

impl<'a> Urn<'a> {
    pub fn parse(value: &'a str) -> Result<Self, CapabilityError> {
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

    pub fn matches(&self, namespace: &str, entity: &str, name: &str, type_id: u32) -> bool {
        self.namespace == namespace
            && self.entity == entity
            && self.name == name
            && self.type_id == type_id
    }

    pub fn standard(&self, entity: &str, name: &str, type_id: u32) -> bool {
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
    pub urn: Urn<'a>,
    pub services: Vec<Service<'a>>,
}

pub(super) struct Service<'a> {
    pub iid: u32,
    pub urn: Urn<'a>,
    pub properties: Vec<Property<'a>>,
}

pub(super) struct Property<'a> {
    pub iid: u32,
    pub urn: Urn<'a>,
    pub value: &'a Value,
}

impl<'a> Description<'a> {
    pub fn parse(value: &'a Value) -> Result<Self, CapabilityError> {
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
            services.push(Service {
                iid,
                urn,
                properties,
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

fn instance_id(value: &Value) -> Result<u32, CapabilityError> {
    value
        .get("iid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(CapabilityError::InvalidDescription("iid"))
}
