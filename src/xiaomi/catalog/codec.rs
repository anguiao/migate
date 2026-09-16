use crate::device::{
    CurtainMovement, DeviceCommand, FeatureCapabilities, FeatureRole, HvacMode, Percent,
    PresenceState, Property, PropertyValue, SwingMode, VacuumCleanMode, VacuumOperationalState,
};

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSpec {
    pub type_urn: String,
    pub features: Vec<FeatureDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FeatureDescriptor {
    pub service_instance: u32,
    pub role: FeatureRole,
    pub name: String,
    pub capabilities: FeatureCapabilities,
    pub properties: Vec<PropertyMapping>,
    pub commands: Vec<CommandMapping>,
    pub events: Vec<EventMapping>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventMapping {
    pub siid: u32,
    pub eiid: u32,
    pub argument_iids: Vec<u32>,
    pub argument_count: usize,
    pub arguments: Vec<EventArgumentMapping>,
    pub(super) effect: EventEffect,
}
#[derive(Clone, Debug, PartialEq)]
pub struct EventArgumentMapping {
    pub index: usize,
    pub mapping: PropertyMapping,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) enum EventEffect {
    Motion(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertyClass {
    Core,
    Ancillary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PropertyMapping {
    pub property: Property,
    pub siid: u32,
    pub piid: u32,
    pub readable: bool,
    pub notify: bool,
    pub class: PropertyClass,
    pub(super) codec: ValueCodec,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandMapping {
    pub command: CommandKind,
    pub target: WireTarget,
    pub(super) codec: ValueCodec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
    Power,
    Brightness,
    ColorTemperature,
    Color,
    TargetTemperature,
    HvacMode,
    FanSpeed,
    SwingMode,
    CurtainPosition,
    CurtainStop,
    Oscillation,
    VacuumStart,
    VacuumStop,
    VacuumDock,
    VacuumCleanMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireTarget {
    Property { siid: u32, piid: u32 },
    Action { siid: u32, aiid: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum WireOperation {
    SetProperty {
        siid: u32,
        piid: u32,
        value: WireValue,
    },
    InvokeAction {
        siid: u32,
        aiid: u32,
        input: Vec<WireValue>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum WireValue {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ValueCodec {
    Bool,
    Percent {
        minimum: f64,
        maximum: f64,
        step: f64,
    },
    IntegerRange {
        minimum: f64,
        maximum: f64,
        step: f64,
    },
    Hvac(Vec<(i64, HvacMode)>),
    FanSpeed(Vec<(i64, u16)>),
    FixedInteger(i64),
    NumberRange {
        minimum: f64,
        maximum: f64,
        step: f64,
    },
    MotionDuration {
        minimum: f64,
        maximum: f64,
        step: f64,
    },
    RgbRange {
        minimum: f64,
        maximum: f64,
        step: f64,
    },
    SwingBool(SwingMode),
    Occupancy {
        vacant: Vec<i64>,
        occupied: Vec<i64>,
    },
    ContactBool,
    ContactEnum {
        open: Vec<i64>,
        closed: Vec<i64>,
    },
    CurtainMovement {
        opening: Vec<i64>,
        closing: Vec<i64>,
        stopped: Vec<i64>,
    },
    VacuumClean(Vec<(i64, VacuumCleanMode)>),
    VacuumState(Vec<(i64, VacuumOperationalState)>),
    Fault,
    Identity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileError {
    InvalidSpec,
    UnsupportedCommand,
    InvalidValue,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSpec => formatter.write_str("invalid MIoT spec"),
            Self::UnsupportedCommand => formatter.write_str("command has no MIoT mapping"),
            Self::InvalidValue => formatter.write_str("value cannot be encoded for MIoT"),
        }
    }
}
impl std::error::Error for CompileError {}

impl FeatureDescriptor {
    pub fn decode(
        &self,
        siid: u32,
        piid: u32,
        value: &WireValue,
    ) -> Option<(Property, Option<PropertyValue>)> {
        self.properties
            .iter()
            .find(|mapping| mapping.siid == siid && mapping.piid == piid)
            .map(|mapping| {
                (
                    mapping.property,
                    mapping.codec.decode(mapping.property, value),
                )
            })
    }

    pub fn encode(&self, command: &DeviceCommand) -> Result<Vec<WireOperation>, CompileError> {
        if let DeviceCommand::SetBrightness(value) = command
            && value.get() == 0.0
            && let Some(mapping) = self
                .commands
                .iter()
                .find(|item| item.command == CommandKind::Power)
        {
            return Ok(vec![mapping.operation(WireValue::Boolean(false))]);
        }
        let (kind, value) = command_value(command);
        let mapping = self
            .commands
            .iter()
            .find(|item| item.command == kind)
            .ok_or(CompileError::UnsupportedCommand)?;
        let value = mapping.codec.encode(command, value)?;
        Ok(vec![mapping.operation(value)])
    }

    pub fn decode_event(
        &self,
        siid: u32,
        eiid: u32,
        arguments: &[WireValue],
    ) -> Option<Vec<(Property, Option<PropertyValue>)>> {
        let mapping = self
            .events
            .iter()
            .find(|mapping| mapping.siid == siid && mapping.eiid == eiid)?;
        if mapping.argument_count != arguments.len() {
            return None;
        }
        let mut updates = match mapping.effect {
            EventEffect::Motion(value) => {
                vec![(Property::Motion, Some(PropertyValue::Motion(value)))]
            }
        };
        updates.extend(mapping.arguments.iter().map(|argument| {
            let mapping = &argument.mapping;
            let value = &arguments[argument.index];
            (
                mapping.property,
                mapping.codec.decode(mapping.property, value),
            )
        }));
        Some(updates)
    }

    pub fn decode_keyed_event(
        &self,
        siid: u32,
        eiid: u32,
        arguments: &[(u32, WireValue)],
    ) -> Option<Vec<(Property, Option<PropertyValue>)>> {
        let mapping = self
            .events
            .iter()
            .find(|mapping| mapping.siid == siid && mapping.eiid == eiid)?;
        if mapping.argument_count != arguments.len() {
            return None;
        }
        let mut ordered = Vec::with_capacity(mapping.argument_iids.len());
        for expected in &mapping.argument_iids {
            let mut matches = arguments
                .iter()
                .filter(|(piid, _)| piid == expected)
                .map(|(_, value)| value);
            let value = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            ordered.push(value.clone());
        }
        if arguments
            .iter()
            .any(|(piid, _)| !mapping.argument_iids.contains(piid))
        {
            return None;
        }
        self.decode_event(siid, eiid, &ordered)
    }
}

impl CommandMapping {
    fn operation(&self, value: WireValue) -> WireOperation {
        match self.target {
            WireTarget::Property { siid, piid } => WireOperation::SetProperty { siid, piid, value },
            WireTarget::Action { siid, aiid } => WireOperation::InvokeAction {
                siid,
                aiid,
                input: if matches!(value, WireValue::String(ref value) if value.is_empty()) {
                    vec![]
                } else {
                    vec![value]
                },
            },
        }
    }
}

fn command_value(command: &DeviceCommand) -> (CommandKind, WireValue) {
    match command {
        DeviceCommand::SetPower(v) => (CommandKind::Power, WireValue::Boolean(*v)),
        DeviceCommand::SetBrightness(v) => (CommandKind::Brightness, WireValue::Number(v.get())),
        DeviceCommand::SetColorTemperature(v) => (
            CommandKind::ColorTemperature,
            WireValue::Integer(i64::from(*v)),
        ),
        DeviceCommand::SetColor(v) => (
            CommandKind::Color,
            WireValue::Integer(
                (i64::from(v.red) << 16) | (i64::from(v.green) << 8) | i64::from(v.blue),
            ),
        ),
        DeviceCommand::SetTargetTemperature(v) => {
            (CommandKind::TargetTemperature, WireValue::Number(*v))
        }
        DeviceCommand::SetHvacMode(v) => {
            (CommandKind::HvacMode, WireValue::String(format!("{v:?}")))
        }
        DeviceCommand::SetFanSpeed(v) => (CommandKind::FanSpeed, WireValue::Integer(i64::from(*v))),
        DeviceCommand::SetSwingMode(v) => {
            (CommandKind::SwingMode, WireValue::String(format!("{v:?}")))
        }
        DeviceCommand::SetCurtainPosition(v) => {
            (CommandKind::CurtainPosition, WireValue::Number(v.get()))
        }
        DeviceCommand::StopCurtain => (CommandKind::CurtainStop, WireValue::String(String::new())),
        DeviceCommand::SetOscillation(v) => (CommandKind::Oscillation, WireValue::Boolean(*v)),
        DeviceCommand::StartVacuum => (CommandKind::VacuumStart, WireValue::String(String::new())),
        DeviceCommand::StopVacuum => (CommandKind::VacuumStop, WireValue::String(String::new())),
        DeviceCommand::ReturnVacuumToDock => {
            (CommandKind::VacuumDock, WireValue::String(String::new()))
        }
        DeviceCommand::SetVacuumCleanMode(v) => (
            CommandKind::VacuumCleanMode,
            WireValue::String(format!("{v:?}")),
        ),
    }
}

impl ValueCodec {
    fn encode(&self, command: &DeviceCommand, value: WireValue) -> Result<WireValue, CompileError> {
        match self {
            Self::Percent {
                minimum,
                maximum,
                step,
            } => {
                let WireValue::Number(value) = value else {
                    return Err(CompileError::InvalidValue);
                };
                let raw = if value == 0.0 {
                    0.0
                } else {
                    (value / 100. * maximum).max(*minimum)
                };
                let raw = minimum + ((raw - minimum) / step).round() * step;
                Ok(numeric_wire(raw))
            }
            Self::IntegerRange {
                minimum,
                maximum,
                step,
            } => {
                let WireValue::Number(value) = value else {
                    return Err(CompileError::InvalidValue);
                };
                if value.fract().abs() > 1e-9 || !valid_number(value, *minimum, *maximum, *step) {
                    return Err(CompileError::InvalidValue);
                }
                Ok(WireValue::Integer(value as i64))
            }
            Self::Hvac(values) => {
                let DeviceCommand::SetHvacMode(mode) = command else {
                    return Err(CompileError::InvalidValue);
                };
                values
                    .iter()
                    .find(|(_, item)| item == mode)
                    .map(|(value, _)| WireValue::Integer(*value))
                    .ok_or(CompileError::InvalidValue)
            }
            Self::FanSpeed(values) => {
                let DeviceCommand::SetFanSpeed(core) = command else {
                    return Err(CompileError::InvalidValue);
                };
                values
                    .iter()
                    .find(|(_, value)| value == core)
                    .map(|(raw, _)| WireValue::Integer(*raw))
                    .ok_or(CompileError::InvalidValue)
            }
            Self::FixedInteger(value) => Ok(WireValue::Integer(*value)),
            Self::NumberRange {
                minimum,
                maximum,
                step,
            } => {
                let raw = number_value(&value).ok_or(CompileError::InvalidValue)?;
                valid_number(raw, *minimum, *maximum, *step)
                    .then_some(value)
                    .ok_or(CompileError::InvalidValue)
            }
            Self::SwingBool(axis) => {
                let DeviceCommand::SetSwingMode(mode) = command else {
                    return Err(CompileError::InvalidValue);
                };
                if *mode != SwingMode::Off && mode != axis {
                    return Err(CompileError::InvalidValue);
                }
                Ok(WireValue::Boolean(*mode != SwingMode::Off))
            }
            Self::VacuumClean(values) => {
                let DeviceCommand::SetVacuumCleanMode(mode) = command else {
                    return Err(CompileError::InvalidValue);
                };
                values
                    .iter()
                    .find(|(_, item)| item == mode)
                    .map(|(value, _)| WireValue::Integer(*value))
                    .ok_or(CompileError::InvalidValue)
            }
            Self::RgbRange {
                minimum,
                maximum,
                step,
            } => {
                let raw = integer_value(&value).ok_or(CompileError::InvalidValue)?;
                valid_number(raw as f64, *minimum, *maximum, *step)
                    .then_some(value)
                    .ok_or(CompileError::InvalidValue)
            }
            _ => Ok(value),
        }
    }
    fn decode(&self, property: Property, value: &WireValue) -> Option<PropertyValue> {
        match self {
            Self::Bool => bool_value(value).and_then(|value| match property {
                Property::Power => Some(PropertyValue::Power(value)),
                Property::Oscillation => Some(PropertyValue::Oscillation(value)),
                _ => None,
            }),
            Self::Percent {
                minimum,
                maximum,
                step,
            } => number_value(value)
                .filter(|raw| {
                    *raw >= *minimum
                        && *raw <= *maximum
                        && ((*raw - *minimum) / *step - ((*raw - *minimum) / *step).round()).abs()
                            < 1e-6
                })
                .and_then(|raw| Percent::new(raw * 100. / maximum).ok())
                .map(PropertyValue::Percent),
            Self::IntegerRange {
                minimum,
                maximum,
                step,
            } => number_value(value)
                .filter(|value| valid_number(*value, *minimum, *maximum, *step))
                .map(PropertyValue::Temperature),
            Self::Hvac(values) => integer_value(value).and_then(|raw| {
                values
                    .iter()
                    .find(|(item, _)| *item == raw)
                    .map(|(_, value)| PropertyValue::HvacMode(*value))
            }),
            Self::FanSpeed(values) => integer_value(value).and_then(|raw| {
                values
                    .iter()
                    .find(|(item, _)| *item == raw)
                    .map(|(_, core)| PropertyValue::FanSpeed(*core))
            }),
            Self::FixedInteger(_) => None,
            Self::NumberRange {
                minimum,
                maximum,
                step,
            } => number_value(value)
                .filter(|value| valid_number(*value, *minimum, *maximum, *step))
                .and_then(|value| match property {
                    Property::CurrentTemperature
                    | Property::Temperature
                    | Property::TargetTemperature => Some(PropertyValue::Temperature(value)),
                    Property::Humidity | Property::Battery => {
                        Percent::new(value).ok().map(PropertyValue::Percent)
                    }
                    Property::Illuminance => Some(PropertyValue::Illuminance(value)),
                    Property::ColorTemperature => u32::try_from(value as i64)
                        .ok()
                        .map(PropertyValue::ColorTemperature),
                    _ => None,
                }),
            Self::MotionDuration {
                minimum,
                maximum,
                step,
            } => number_value(value)
                .filter(|value| valid_number(*value, *minimum, *maximum, *step))
                .map(|_| PropertyValue::Motion(false)),
            Self::RgbRange {
                minimum,
                maximum,
                step,
            } => integer_value(value)
                .filter(|value| valid_number(*value as f64, *minimum, *maximum, *step))
                .and_then(|value| u32::try_from(value).ok())
                .map(|value| {
                    PropertyValue::Color(crate::device::RgbColor {
                        red: (value >> 16) as u8,
                        green: (value >> 8) as u8,
                        blue: value as u8,
                    })
                }),
            Self::SwingBool(axis) => bool_value(value)
                .map(|v| PropertyValue::SwingMode(if v { *axis } else { SwingMode::Off })),
            Self::Occupancy { vacant, occupied } => integer_value(value).and_then(|raw| {
                if vacant.contains(&raw) {
                    Some(PropertyValue::Occupancy(PresenceState::Vacant))
                } else if occupied.contains(&raw) {
                    Some(PropertyValue::Occupancy(PresenceState::Occupied))
                } else {
                    None
                }
            }),
            Self::ContactBool => bool_value(value).map(PropertyValue::ContactOpen),
            Self::ContactEnum { open, closed } => integer_value(value).and_then(|raw| {
                if open.contains(&raw) {
                    Some(PropertyValue::ContactOpen(true))
                } else if closed.contains(&raw) {
                    Some(PropertyValue::ContactOpen(false))
                } else {
                    None
                }
            }),
            Self::CurtainMovement {
                opening,
                closing,
                stopped,
            } => integer_value(value).and_then(|raw| {
                if opening.contains(&raw) {
                    Some(PropertyValue::CurtainMovement(CurtainMovement::Opening))
                } else if closing.contains(&raw) {
                    Some(PropertyValue::CurtainMovement(CurtainMovement::Closing))
                } else if stopped.contains(&raw) {
                    Some(PropertyValue::CurtainMovement(CurtainMovement::Stopped))
                } else {
                    None
                }
            }),
            Self::VacuumClean(values) => integer_value(value).and_then(|raw| {
                values
                    .iter()
                    .find(|(item, _)| *item == raw)
                    .map(|(_, value)| PropertyValue::VacuumCleanMode(*value))
            }),
            Self::VacuumState(values) => integer_value(value).and_then(|raw| {
                values
                    .iter()
                    .find(|(item, _)| *item == raw)
                    .map(|(_, value)| PropertyValue::VacuumOperationalState(*value))
            }),
            Self::Fault => Some(PropertyValue::VacuumFault(match value {
                WireValue::String(value) => value.clone(),
                WireValue::Integer(value) => value.to_string(),
                _ => return None,
            })),
            Self::Identity => None,
        }
    }
}

fn number_value(value: &WireValue) -> Option<f64> {
    match value {
        WireValue::Integer(v) => Some(*v as f64),
        WireValue::Number(v) if v.is_finite() => Some(*v),
        _ => None,
    }
}
fn integer_value(value: &WireValue) -> Option<i64> {
    match value {
        WireValue::Integer(v) => Some(*v),
        _ => None,
    }
}
fn bool_value(value: &WireValue) -> Option<bool> {
    match value {
        WireValue::Boolean(v) => Some(*v),
        _ => None,
    }
}
fn numeric_wire(value: f64) -> WireValue {
    if value.fract().abs() < 1e-9 {
        WireValue::Integer(value as i64)
    } else {
        WireValue::Number(value)
    }
}
fn valid_number(value: f64, minimum: f64, maximum: f64, step: f64) -> bool {
    value.is_finite()
        && value >= minimum
        && value <= maximum
        && step > 0.
        && ((value - minimum) / step - ((value - minimum) / step).round()).abs() < 1e-6
}
