use super::codec::{CompileError, ValueCodec};
use crate::device::{DeviceCommand, Property, PropertyValue};

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSpec {
    pub type_urn: String,
    pub features: Vec<FeatureDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FeatureDescriptor {
    pub definition: crate::device::FeatureDefinition,
    pub binding: XiaomiBinding,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct XiaomiBinding {
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
    ArgumentsOnly,
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

impl XiaomiBinding {
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
            EventEffect::ArgumentsOnly => Vec::new(),
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
