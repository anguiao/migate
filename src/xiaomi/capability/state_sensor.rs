//! Native binary-state semantics and the required motion event definition.
use super::{
    CapabilityError, NumericFormat, ParsedFunction, PropertyBinding,
    description::{Description, Property, Service, array, string},
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventBinding {
    pub siid: u32,
    pub eiid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventArgument {
    pub property: PropertyBinding,
    pub format: String,
    pub unit: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateValue {
    pub native: i64,
    pub active: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateMapping {
    ContactBoolean,
    Enumerated {
        format: NumericFormat,
        values: Vec<StateValue>,
    },
    NoMotionDuration {
        format: NumericFormat,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateBinding {
    property: PropertyBinding,
    mapping: StateMapping,
}
impl StateBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        mapping: StateMapping,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0 || property.piid == 0 {
            return Err(CapabilityError::InvalidDescription("state binding"));
        }
        match &mapping {
            StateMapping::ContactBoolean => {}
            StateMapping::NoMotionDuration { format } if *format != NumericFormat::Float => {}
            StateMapping::Enumerated { format, values } if *format != NumericFormat::Float => {
                let mut seen = std::collections::HashSet::new();
                if !values
                    .iter()
                    .all(|value| integer_fits(*format, value.native) && seen.insert(value.native))
                    || !values.iter().any(|value| value.active == Some(true))
                    || !values.iter().any(|value| value.active == Some(false))
                {
                    return Err(CapabilityError::InvalidDescription("state enumeration"));
                }
            }
            _ => return Err(CapabilityError::InvalidDescription("state format")),
        }
        Ok(Self { property, mapping })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn mapping(&self) -> &StateMapping {
        &self.mapping
    }
    pub fn read_value(&self, value: &Value) -> Result<Option<bool>, CapabilityError> {
        let invalid = || CapabilityError::InvalidValue("state");
        match &self.mapping {
            StateMapping::ContactBoolean => value
                .as_bool()
                .map(|value| Some(!value))
                .ok_or_else(invalid),
            StateMapping::NoMotionDuration { format } => {
                let raw = value
                    .as_i64()
                    .filter(|raw| *raw >= 0 && integer_fits(*format, *raw))
                    .ok_or_else(invalid)?;
                Ok(Some(raw == 0))
            }
            StateMapping::Enumerated { format, values } => {
                let raw = value
                    .as_i64()
                    .filter(|raw| integer_fits(*format, *raw))
                    .ok_or_else(invalid)?;
                values
                    .iter()
                    .find(|value| value.native == raw)
                    .map(|value| value.active)
                    .ok_or_else(invalid)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MotionBinding {
    state: StateBinding,
    event: EventBinding,
    arguments: Vec<EventArgument>,
}
impl MotionBinding {
    pub(crate) fn new(
        state: StateBinding,
        event: EventBinding,
        arguments: Vec<EventArgument>,
    ) -> Result<Self, CapabilityError> {
        let mut seen = std::collections::HashSet::new();
        if event.siid != state.property().siid
            || event.eiid == 0
            || !matches!(state.mapping(), StateMapping::NoMotionDuration { .. })
            || arguments.iter().any(|argument| {
                argument.property.siid != event.siid
                    || argument.property.piid == 0
                    || argument.format.is_empty()
                    || !seen.insert(argument.property)
            })
        {
            return Err(CapabilityError::InvalidDescription("motion binding"));
        }
        Ok(Self {
            state,
            event,
            arguments,
        })
    }
    pub fn state(&self) -> &StateBinding {
        &self.state
    }
    pub fn event(&self) -> EventBinding {
        self.event
    }
    pub fn arguments(&self) -> &[EventArgument] {
        &self.arguments
    }
}

fn integer_fits(format: NumericFormat, raw: i64) -> bool {
    match format {
        NumericFormat::Uint8 => u8::try_from(raw).is_ok(),
        NumericFormat::Uint16 => u16::try_from(raw).is_ok(),
        NumericFormat::Uint32 => u32::try_from(raw).is_ok(),
        NumericFormat::Int8 => i8::try_from(raw).is_ok(),
        NumericFormat::Int16 => i16::try_from(raw).is_ok(),
        NumericFormat::Int32 => i32::try_from(raw).is_ok(),
        NumericFormat::Float => false,
    }
}
fn integer_format(value: &Value) -> Result<NumericFormat, CapabilityError> {
    match string(value, "format")? {
        "uint8" => Ok(NumericFormat::Uint8),
        "uint16" => Ok(NumericFormat::Uint16),
        "uint32" => Ok(NumericFormat::Uint32),
        "int8" => Ok(NumericFormat::Int8),
        "int16" => Ok(NumericFormat::Int16),
        "int32" => Ok(NumericFormat::Int32),
        _ => Err(CapabilityError::InvalidDescription("state format")),
    }
}
fn readable_notification(property: &Property<'_>) -> Result<(), CapabilityError> {
    let access = array(property.value, "access")?;
    if access.iter().any(|value| value.as_str().is_none()) {
        return Err(CapabilityError::InvalidDescription("access"));
    }
    for permission in ["read", "notify"] {
        if !access
            .iter()
            .any(|value| value.as_str() == Some(permission))
        {
            return Err(CapabilityError::MissingAccess(permission));
        }
    }
    Ok(())
}
fn enumeration(
    property: &Property<'_>,
    active: &[&str],
    inactive: &[&str],
    allow_unknown: bool,
) -> Result<StateMapping, CapabilityError> {
    let values = array(property.value, "value-list")?
        .iter()
        .map(|entry| {
            let description = string(entry, "description")?;
            let active = if active.contains(&description) {
                Some(true)
            } else if inactive.contains(&description) {
                Some(false)
            } else if allow_unknown {
                None
            } else {
                return Err(CapabilityError::InvalidDescription("contact enumeration"));
            };
            Ok(StateValue {
                native: entry["value"]
                    .as_i64()
                    .ok_or(CapabilityError::InvalidDescription("state enumeration"))?,
                active,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(StateMapping::Enumerated {
        format: integer_format(property.value)?,
        values,
    })
}
fn contact(service: &Service<'_>) -> Result<StateBinding, CapabilityError> {
    let mut failure = CapabilityError::MissingFunction("contact state");
    for (name, type_id) in [("contact-state", 0x7C), ("status", 0x7)] {
        for property in service
            .properties
            .iter()
            .filter(|property| property.urn.standard("property", name, type_id))
        {
            let candidate = (|| {
                readable_notification(property)?;
                let mapping = if name == "contact-state" {
                    if property.value["format"] != "bool" {
                        return Err(CapabilityError::InvalidDescription("contact format"));
                    }
                    StateMapping::ContactBoolean
                } else {
                    enumeration(property, &["Open"], &["Closed"], false)?
                };
                StateBinding::new(
                    PropertyBinding {
                        siid: service.iid,
                        piid: property.iid,
                    },
                    mapping,
                )
            })();
            match candidate {
                Ok(binding) => return Ok(binding),
                Err(error) => failure = error,
            }
        }
    }
    Err(failure)
}
fn motion(service: &Service<'_>) -> Result<MotionBinding, CapabilityError> {
    let event = service
        .events
        .iter()
        .find(|event| event.urn.standard("event", "motion-detected", 0x5001))
        .ok_or(CapabilityError::MissingFunction("motion event"))?;
    let property = service
        .properties
        .iter()
        .find(|property| {
            property
                .urn
                .standard("property", "no-motion-duration", 0xCB)
        })
        .ok_or(CapabilityError::MissingFunction("no-motion-duration"))?;
    readable_notification(property)?;
    if property.value["unit"] != "seconds" {
        return Err(CapabilityError::InvalidDescription("motion duration unit"));
    }
    let state = StateBinding::new(
        PropertyBinding {
            siid: service.iid,
            piid: property.iid,
        },
        StateMapping::NoMotionDuration {
            format: integer_format(property.value)?,
        },
    )?;
    let arguments = event
        .arguments
        .iter()
        .map(|piid| {
            let property = service
                .properties
                .iter()
                .find(|property| property.iid == *piid)
                .unwrap();
            Ok(EventArgument {
                property: PropertyBinding {
                    siid: service.iid,
                    piid: *piid,
                },
                format: string(property.value, "format")?.to_owned(),
                unit: property.value["unit"].as_str().map(str::to_owned),
            })
        })
        .collect::<Result<_, CapabilityError>>()?;
    MotionBinding::new(
        state,
        EventBinding {
            siid: service.iid,
            eiid: event.iid,
        },
        arguments,
    )
}
pub(super) fn functions(
    description: &Description<'_>,
    diagnostics: &mut Vec<super::RecognitionDiagnostic>,
) -> Vec<ParsedFunction> {
    let mut result = Vec::new();
    for service in description
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "magnet-sensor", 0x7827))
    {
        let binding = match contact(service) {
            Ok(binding) => binding,
            Err(error) => {
                diagnostics.push(super::RecognitionDiagnostic {
                    target: format!("contact:{}", service.iid),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let property = binding.property();
        result.push(ParsedFunction::contact(
            format!("contact:{}:{}", property.siid, property.piid),
            "Contact",
            binding,
        ));
    }
    for service in description
        .services
        .iter()
        .filter(|service| service.urn.standard("service", "motion-sensor", 0x7825))
    {
        let binding = match motion(service) {
            Ok(binding) => binding,
            Err(error) => {
                diagnostics.push(super::RecognitionDiagnostic {
                    target: format!("motion:{}", service.iid),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let property = binding.state().property();
        result.push(ParsedFunction::motion_sensor(
            format!("motion:{}:{}", property.siid, property.piid),
            "Motion",
            binding,
        ));
    }
    if let Some(service) = description
        .services
        .iter()
        .find(|service| service.urn.standard("service", "occupancy-sensor", 0x78C0))
    {
        let candidate = (|| {
            let property = service
                .properties
                .iter()
                .find(|property| property.urn.standard("property", "occupancy-status", 0x166))
                .ok_or(CapabilityError::MissingFunction("occupancy status"))?;
            readable_notification(property)?;
            let mapping = enumeration(property, &["Has One", "Has Someone"], &["No One"], true)?;
            StateBinding::new(
                PropertyBinding {
                    siid: service.iid,
                    piid: property.iid,
                },
                mapping,
            )
        })();
        match candidate {
            Ok(binding) => {
                let property = binding.property();
                result.push(ParsedFunction::occupancy(
                    format!("occupancy:{}:{}", property.siid, property.piid),
                    "Occupancy",
                    binding,
                ));
            }
            Err(error) => diagnostics.push(super::RecognitionDiagnostic {
                target: format!("occupancy:{}", service.iid),
                reason: error.to_string(),
            }),
        }
    }
    result
}
