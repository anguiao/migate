use super::codec::*;
use super::compiler::{ActionSpec, PropertySpec, Service, numeric_format};
use crate::device::{
    Capability, FeatureCapabilities, FeatureRole, HvacMode, NumericRange, NumericUnit, Property,
    SensingModality, SwingMode, VacuumCleanMode, VacuumOperationalState,
};

pub(super) fn compile_lights(
    services: &[Service<'_>],
    role: FeatureRole,
    output: &mut Vec<FeatureDescriptor>,
) {
    for service in services.iter().filter(|service| service.kind == "light") {
        if let Some(mut feature) = powered_feature(service, role) {
            add_numeric(
                service,
                "brightness",
                Property::Brightness,
                CapabilityKind::Brightness,
                ValueCodecKind::Percent,
                &mut feature,
            );
            add_numeric(
                service,
                "color-temperature",
                Property::ColorTemperature,
                CapabilityKind::ColorTemperature,
                ValueCodecKind::Identity,
                &mut feature,
            );
            if let Some(color) = writable_property(service, "color").filter(|property| {
                property.format == "uint32"
                    && property.unit == Some("rgb")
                    && property.range.is_some_and(|(minimum, maximum, step)| {
                        minimum >= 0.
                            && minimum <= maximum
                            && maximum <= 0x00ff_ffff as f64
                            && minimum.fract() == 0.
                            && maximum.fract() == 0.
                            && step > 0.
                            && step.fract() == 0.
                    })
            }) && let Some((minimum, maximum, step)) = color.range
            {
                let codec = ValueCodec::RgbRange {
                    minimum,
                    maximum,
                    step,
                };
                feature.capabilities.0.push(Capability::Color);
                feature.commands.push(command(
                    service.iid,
                    color,
                    CommandKind::Color,
                    codec.clone(),
                ));
                feature
                    .properties
                    .push(mapping(service.iid, color, Property::Color, codec));
            }
            output.push(feature);
        }
    }
}
pub(super) fn compile_loads(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    for service in services
        .iter()
        .filter(|service| matches!(service.kind, "switch" | "outlet"))
    {
        if let Some(feature) = powered_feature(service, FeatureRole::Load) {
            output.push(feature);
        }
    }
}
pub(super) fn compile_climate(
    model: &str,
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "air-conditioner")
    else {
        return;
    };
    let Some(mut feature) = powered_feature(service, FeatureRole::Climate) else {
        return;
    };
    if let Some(property) = writable_property(service, "target-temperature")
        && let Some(range) = numeric_range(property, NumericUnit::Celsius)
    {
        feature
            .capabilities
            .0
            .push(Capability::TargetTemperature(range));
        // These companion firmwares require integer targets despite float spec metadata.
        let codec = if matches!(model, "lumi.acpartner.mcn02" | "lumi.acpartner.mcn04") {
            ValueCodec::IntegerRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            }
        } else {
            temperature_command_codec(property, range)
        };
        feature.properties.push(mapping(
            service.iid,
            property,
            Property::TargetTemperature,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
        feature.commands.push(command(
            service.iid,
            property,
            CommandKind::TargetTemperature,
            codec,
        ));
    }
    if let Some(property) = readable_property(service, "temperature")
        && let Some(range) = numeric_range(property, NumericUnit::Celsius)
    {
        feature.capabilities.0.push(Capability::Temperature(range));
        feature.properties.push(mapping(
            service.iid,
            property,
            Property::CurrentTemperature,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
    if let Some(property) = writable_property(service, "mode") {
        let values = enum_map(&property.values, hvac_mode);
        if !values.is_empty() {
            feature.capabilities.0.push(Capability::HvacModes(
                values.iter().map(|(_, value)| *value).collect(),
            ));
            feature.properties.push(mapping(
                service.iid,
                property,
                Property::HvacMode,
                ValueCodec::Hvac(values.clone()),
            ));
            feature.commands.push(command(
                service.iid,
                property,
                CommandKind::HvacMode,
                ValueCodec::Hvac(values),
            ));
        }
    }
    if let Some(fan) = services
        .iter()
        .find(|service| service.kind == "fan-control")
    {
        if let Some(property) = writable_property(fan, "fan-level") {
            let values = fan_levels(property);
            feature.capabilities.0.push(Capability::FanSpeeds(
                values.iter().map(|(_, value)| *value).collect(),
            ));
            feature.properties.push(mapping(
                fan.iid,
                property,
                Property::FanSpeed,
                ValueCodec::FanSpeed(values.clone()),
            ));
            feature.commands.push(command(
                fan.iid,
                property,
                CommandKind::FanSpeed,
                ValueCodec::FanSpeed(values),
            ));
        }
        if let Some(property) = writable_property(fan, "vertical-swing") {
            feature.capabilities.0.push(Capability::SwingModes(vec![
                SwingMode::Off,
                SwingMode::Vertical,
            ]));
            feature.properties.push(mapping(
                fan.iid,
                property,
                Property::SwingMode,
                ValueCodec::SwingBool(SwingMode::Vertical),
            ));
            feature.commands.push(command(
                fan.iid,
                property,
                CommandKind::SwingMode,
                ValueCodec::SwingBool(SwingMode::Vertical),
            ));
        }
    }
    output.push(feature);
}
pub(super) fn compile_curtain(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    let Some(service) = services.iter().find(|service| service.kind == "curtain") else {
        return;
    };
    let Some(target) = writable_property(service, "target-position") else {
        return;
    };
    let Some(range) = numeric_range(target, NumericUnit::Percent) else {
        return;
    };
    let codec = ValueCodec::Percent {
        minimum: range.minimum,
        maximum: range.maximum,
        step: range.step,
    };
    let mut feature = base_feature(service, FeatureRole::Curtain);
    feature
        .capabilities
        .0
        .push(Capability::CurtainPosition(NumericRange {
            minimum: 0.,
            maximum: 100.,
            step: 1.,
            unit: NumericUnit::Percent,
        }));
    feature.commands.push(command(
        service.iid,
        target,
        CommandKind::CurtainPosition,
        codec.clone(),
    ));
    feature.properties.push(mapping(
        service.iid,
        target,
        Property::CurtainTargetPosition,
        codec.clone(),
    ));
    if let Some(current) = readable_property(service, "current-position")
        && let Some(current_range) = numeric_range(current, NumericUnit::Percent)
    {
        feature.properties.push(mapping(
            service.iid,
            current,
            Property::CurtainPosition,
            ValueCodec::Percent {
                minimum: current_range.minimum,
                maximum: current_range.maximum,
                step: current_range.step,
            },
        ));
    }
    if let Some(status) = readable_property(service, "status") {
        feature.properties.push(mapping(
            service.iid,
            status,
            Property::CurtainMovement,
            ValueCodec::CurtainMovement {
                opening: enum_values(status, &["opening"]),
                closing: enum_values(status, &["closing"]),
                stopped: enum_values(status, &["stop", "stopped"]),
            },
        ));
    }
    if let Some(control) = writable_property(service, "motor-control")
        && let Some(stop) = enum_value(control, &["pause", "stop"])
    {
        feature.capabilities.0.push(Capability::CurtainStop);
        feature.commands.push(CommandMapping {
            command: CommandKind::CurtainStop,
            target: WireTarget::Property {
                siid: service.iid,
                piid: control.iid,
            },
            codec: ValueCodec::FixedInteger(stop),
        });
    }
    output.push(feature);
}
pub(super) fn compile_fan(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    let Some(service) = services.iter().find(|service| service.kind == "fan") else {
        return;
    };
    let Some(mut feature) = powered_feature(service, FeatureRole::Fan) else {
        return;
    };
    if let Some(property) =
        writable_property(service, "fan-level").filter(|property| integer_format(property.format))
    {
        let values = fan_levels(property);
        if !values.is_empty() {
            feature.capabilities.0.push(Capability::FanSpeeds(
                values.iter().map(|(_, value)| *value).collect(),
            ));
            feature.properties.push(mapping(
                service.iid,
                property,
                Property::FanSpeed,
                ValueCodec::FanSpeed(values.clone()),
            ));
            feature.commands.push(command(
                service.iid,
                property,
                CommandKind::FanSpeed,
                ValueCodec::FanSpeed(values),
            ));
        }
    }
    if let Some(property) = writable_property(service, "horizontal-swing")
        .filter(|property| property.format == "bool")
        .or_else(|| {
            writable_property(service, "vertical-swing")
                .filter(|property| property.format == "bool")
        })
    {
        let axis = if property.kind == "vertical-swing" {
            SwingMode::Vertical
        } else {
            SwingMode::Horizontal
        };
        feature
            .capabilities
            .0
            .push(Capability::SwingModes(vec![SwingMode::Off, axis]));
        feature.properties.push(mapping(
            service.iid,
            property,
            Property::Oscillation,
            ValueCodec::Bool,
        ));
        feature.commands.push(command(
            service.iid,
            property,
            CommandKind::Oscillation,
            ValueCodec::Bool,
        ));
    }
    output.push(feature);
}
pub(super) fn compile_temperature_humidity(
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "temperature-humidity-sensor")
    else {
        return;
    };
    for (kind, property, role, capability, unit) in [
        (
            "temperature",
            Property::Temperature,
            FeatureRole::TemperatureSensor,
            CapabilityKind::Temperature,
            NumericUnit::Celsius,
        ),
        (
            "relative-humidity",
            Property::Humidity,
            FeatureRole::HumiditySensor,
            CapabilityKind::Humidity,
            NumericUnit::Percent,
        ),
    ] {
        if let Some(wire) = readable_property(service, kind)
            && let Some(range) = numeric_range(wire, unit)
        {
            let mut feature = base_feature(service, role);
            feature.capabilities.0.push(capability.value(range));
            feature.properties.push(mapping(
                service.iid,
                wire,
                property,
                ValueCodec::NumberRange {
                    minimum: range.minimum,
                    maximum: range.maximum,
                    step: range.step,
                },
            ));
            attach_battery(services, &mut feature);
            output.push(feature);
        }
    }
}
pub(super) fn compile_motion(
    model: &str,
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) -> Result<(), CompileError> {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "motion-sensor")
    else {
        return Ok(());
    };
    if !service
        .events
        .iter()
        .any(|event| event.kind == "motion-detected")
    {
        return Ok(());
    }
    let mut feature = base_feature(service, FeatureRole::MotionSensor);
    feature.capabilities.0.push(Capability::Motion);
    feature
        .capabilities
        .0
        .push(Capability::SensingModalities(sensing_modalities(model)));
    if let Some(duration) = readable_property(service, "no-motion-duration")
        .filter(|property| property.unit == Some("seconds"))
        && let Some((minimum, maximum, step)) = duration.range
        && minimum > 0.
        && maximum >= minimum
        && step > 0.
    {
        feature.properties.push(mapping(
            service.iid,
            duration,
            Property::Motion,
            ValueCodec::MotionDuration {
                minimum,
                maximum,
                step,
            },
        ));
    }
    let mut illuminance = base_feature(service, FeatureRole::IlluminanceSensor);
    if let Some(illumination) =
        readable_property(service, "illumination").filter(|property| property.unit == Some("lux"))
        && let Some(range) = numeric_range(illumination, NumericUnit::Lux)
    {
        illuminance
            .capabilities
            .0
            .push(Capability::Illuminance(range));
        illuminance.properties.push(mapping(
            service.iid,
            illumination,
            Property::Illuminance,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
    attach_battery(services, &mut feature);
    for event in service
        .events
        .iter()
        .filter(|event| event.kind == "motion-detected")
    {
        let arguments = event_argument_mappings(service, event, PropertyClass::Core);
        for argument in &arguments {
            if argument.mapping.property == Property::Illuminance
                && !illuminance
                    .capabilities
                    .0
                    .iter()
                    .any(|capability| matches!(capability, Capability::Illuminance(_)))
                && let ValueCodec::NumberRange {
                    minimum,
                    maximum,
                    step,
                } = argument.mapping.codec
            {
                illuminance
                    .capabilities
                    .0
                    .push(Capability::Illuminance(NumericRange {
                        minimum,
                        maximum,
                        step,
                        unit: NumericUnit::Lux,
                    }));
            }
        }
        feature.events.push(EventMapping {
            siid: service.iid,
            eiid: event.iid,
            argument_iids: event.arguments.clone(),
            argument_count: event.arguments.len(),
            effect: EventEffect::Motion(true),
            arguments: arguments
                .iter()
                .filter(|argument| argument.mapping.property != Property::Illuminance)
                .cloned()
                .collect(),
        });
        let lux_arguments = arguments
            .into_iter()
            .filter(|argument| argument.mapping.property == Property::Illuminance)
            .collect::<Vec<_>>();
        if !lux_arguments.is_empty() {
            illuminance.events.push(EventMapping {
                siid: service.iid,
                eiid: event.iid,
                argument_iids: event.arguments.clone(),
                argument_count: event.arguments.len(),
                effect: EventEffect::ArgumentsOnly,
                arguments: lux_arguments,
            });
        }
    }
    if model == "xiaomi.motion.pir1"
        && let Some(custom) = services.iter().find(|service| service.iid == 5)
        && let Some(event) = custom.events.iter().find(|event| event.iid == 1022)
    {
        feature.events.push(EventMapping {
            siid: custom.iid,
            eiid: event.iid,
            argument_iids: event.arguments.clone(),
            argument_count: event.arguments.len(),
            effect: EventEffect::Motion(false),
            arguments: vec![],
        });
    }
    output.push(feature);
    if illuminance
        .capabilities
        .0
        .iter()
        .any(|capability| matches!(capability, Capability::Illuminance(_)))
    {
        attach_battery(services, &mut illuminance);
        output.push(illuminance);
    }
    Ok(())
}
pub(super) fn compile_occupancy(
    model: &str,
    services: &[Service<'_>],
    output: &mut Vec<FeatureDescriptor>,
) {
    let Some(service) = services
        .iter()
        .filter(|service| service.kind == "occupancy-sensor")
        .min_by_key(|service| service.iid)
    else {
        return;
    };
    let Some(status) = readable_property(service, "occupancy-status") else {
        return;
    };
    let mut feature = base_feature(service, FeatureRole::OccupancySensor);
    feature.capabilities.0.push(Capability::Occupancy);
    feature
        .capabilities
        .0
        .push(Capability::SensingModalities(sensing_modalities(model)));
    feature.properties.push(mapping(
        service.iid,
        status,
        Property::Occupancy,
        ValueCodec::Occupancy {
            vacant: enum_values(status, &["no one", "noshow", "vacant"]),
            occupied: enum_values(status, &["has one", "show", "occupied"]),
        },
    ));
    let mut illuminance = base_feature(service, FeatureRole::IlluminanceSensor);
    if let Some(illumination) =
        readable_property(service, "illumination").filter(|property| property.unit == Some("lux"))
        && let Some(range) = numeric_range(illumination, NumericUnit::Lux)
    {
        illuminance
            .capabilities
            .0
            .push(Capability::Illuminance(range));
        illuminance.properties.push(mapping(
            service.iid,
            illumination,
            Property::Illuminance,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
    attach_battery(services, &mut feature);
    output.push(feature);
    if !illuminance.properties.is_empty() {
        attach_battery(services, &mut illuminance);
        output.push(illuminance);
    }
}
pub(super) fn compile_contact(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "magnet-sensor")
    else {
        return;
    };
    let Some(status) = readable_property(service, "contact-state")
        .or_else(|| readable_property(service, "status"))
    else {
        return;
    };
    let codec = if status.format == "bool" {
        ValueCodec::ContactBool
    } else {
        ValueCodec::ContactEnum {
            open: enum_values(status, &["open"]),
            closed: enum_values(status, &["closed", "close"]),
        }
    };
    let mut feature = base_feature(service, FeatureRole::ContactSensor);
    feature.capabilities.0.push(Capability::Contact);
    feature
        .properties
        .push(mapping(service.iid, status, Property::Contact, codec));
    attach_battery(services, &mut feature);
    output.push(feature);
}
pub(super) fn compile_vacuum(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    let Some(service) = services.iter().find(|service| service.kind == "vacuum") else {
        return;
    };
    let start = service
        .actions
        .iter()
        .find(|action| action.kind == "start-sweep" && action.arguments.is_empty());
    let stop = service
        .actions
        .iter()
        .find(|action| action.kind == "stop-sweeping" && action.arguments.is_empty());
    if start.is_none() || stop.is_none() {
        return;
    }
    let mut feature = base_feature(service, FeatureRole::Vacuum);
    feature.capabilities.0.push(Capability::VacuumControl);
    for (kind, action) in [
        (CommandKind::VacuumStart, start.unwrap()),
        (CommandKind::VacuumStop, stop.unwrap()),
    ] {
        feature
            .commands
            .push(action_command(service.iid, action.iid, kind));
    }
    if let Some(battery) = services.iter().find(|item| item.kind == "battery")
        && let Some(action) = battery
            .actions
            .iter()
            .find(|action| action.kind == "start-charge" && action.arguments.is_empty())
    {
        feature.capabilities.0.push(Capability::VacuumDock);
        feature.commands.push(action_command(
            battery.iid,
            action.iid,
            CommandKind::VacuumDock,
        ));
    }
    if let Some(mode) = writable_property(service, "mode") {
        let values = enum_map(&mode.values, vacuum_mode);
        if !values.is_empty() {
            feature.capabilities.0.push(Capability::VacuumCleanModes(
                values.iter().map(|(_, v)| *v).collect(),
            ));
            feature.properties.push(mapping(
                service.iid,
                mode,
                Property::VacuumCleanMode,
                ValueCodec::VacuumClean(values.clone()),
            ));
            feature.commands.push(command(
                service.iid,
                mode,
                CommandKind::VacuumCleanMode,
                ValueCodec::VacuumClean(values),
            ));
        }
    }
    if let Some(status) = readable_property(service, "status") {
        feature.properties.push(mapping(
            service.iid,
            status,
            Property::VacuumOperationalState,
            ValueCodec::VacuumState(enum_map(&status.values, vacuum_state)),
        ));
    }
    if let Some(fault) = readable_property(service, "fault") {
        feature.properties.push(mapping(
            service.iid,
            fault,
            Property::VacuumFault,
            ValueCodec::Fault,
        ));
    }
    attach_battery(services, &mut feature);
    output.push(feature);
}
pub(super) fn compile_bath_heater(services: &[Service<'_>], output: &mut Vec<FeatureDescriptor>) {
    compile_lights(services, FeatureRole::BathHeaterLight, output);
    let Some(service) = services
        .iter()
        .find(|service| service.kind == "ptc-bath-heater")
    else {
        return;
    };
    for (kind, role) in [
        ("blow", FeatureRole::BathHeaterSupplyFan),
        ("ventilation", FeatureRole::BathHeaterExhaustFan),
    ] {
        if let Some(property) =
            writable_property(service, kind).filter(|property| property.format == "bool")
        {
            let mut feature = base_feature(service, role);
            feature
                .capabilities
                .0
                .push(Capability::Power { writable: true });
            feature.properties.push(mapping(
                service.iid,
                property,
                Property::Power,
                ValueCodec::Bool,
            ));
            feature.commands.push(command(
                service.iid,
                property,
                CommandKind::Power,
                ValueCodec::Bool,
            ));
            output.push(feature);
        }
    }
    if let Some(heating) =
        writable_property(service, "heating").filter(|property| property.format == "bool")
    {
        let mut feature = base_feature(service, FeatureRole::BathHeaterClimate);
        feature
            .capabilities
            .0
            .push(Capability::Power { writable: true });
        feature.properties.push(mapping(
            service.iid,
            heating,
            Property::Power,
            ValueCodec::Bool,
        ));
        feature.commands.push(command(
            service.iid,
            heating,
            CommandKind::Power,
            ValueCodec::Bool,
        ));
        if let Some(target) = writable_property(service, "target-temperature")
            && let Some(range) = numeric_range(target, NumericUnit::Celsius)
        {
            feature
                .capabilities
                .0
                .push(Capability::TargetTemperature(range));
            feature.properties.push(mapping(
                service.iid,
                target,
                Property::TargetTemperature,
                ValueCodec::NumberRange {
                    minimum: range.minimum,
                    maximum: range.maximum,
                    step: range.step,
                },
            ));
            feature.commands.push(command(
                service.iid,
                target,
                CommandKind::TargetTemperature,
                temperature_command_codec(target, range),
            ));
        }
        output.push(feature);
    }
}

fn powered_feature(service: &Service<'_>, role: FeatureRole) -> Option<FeatureDescriptor> {
    let power = writable_property(service, "on").filter(|property| property.format == "bool")?;
    let mut feature = base_feature(service, role);
    feature
        .capabilities
        .0
        .push(Capability::Power { writable: true });
    feature.properties.push(mapping(
        service.iid,
        power,
        Property::Power,
        ValueCodec::Bool,
    ));
    feature.commands.push(command(
        service.iid,
        power,
        CommandKind::Power,
        ValueCodec::Bool,
    ));
    Some(feature)
}
fn base_feature(service: &Service<'_>, role: FeatureRole) -> FeatureDescriptor {
    FeatureDescriptor {
        service_instance: service.iid,
        role,
        name: service.name.to_owned(),
        capabilities: FeatureCapabilities::default(),
        properties: vec![],
        commands: vec![],
        events: vec![],
    }
}
fn attach_battery(services: &[Service<'_>], feature: &mut FeatureDescriptor) {
    let Some(service) = services.iter().find(|service| service.kind == "battery") else {
        return;
    };
    let Some(property) = readable_property(service, "battery-level") else {
        return;
    };
    if let Some(range) = numeric_range(property, NumericUnit::Percent) {
        feature.capabilities.0.push(Capability::Battery(range));
        feature.properties.push(mapping(
            service.iid,
            property,
            Property::Battery,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
    }
}
#[derive(Clone, Copy)]
enum CapabilityKind {
    Brightness,
    ColorTemperature,
    Temperature,
    Humidity,
}
impl CapabilityKind {
    fn value(self, range: NumericRange) -> Capability {
        match self {
            Self::Brightness => Capability::Brightness(range),
            Self::ColorTemperature => Capability::ColorTemperature(range),
            Self::Temperature => Capability::Temperature(range),
            Self::Humidity => Capability::Humidity(range),
        }
    }
}
#[derive(Clone, Copy)]
enum ValueCodecKind {
    Percent,
    Identity,
}
fn add_numeric(
    service: &Service<'_>,
    kind: &str,
    property: Property,
    capability: CapabilityKind,
    codec: ValueCodecKind,
    feature: &mut FeatureDescriptor,
) {
    let Some(wire) = writable_property(service, kind) else {
        return;
    };
    let unit = match capability {
        CapabilityKind::Brightness => NumericUnit::Percent,
        CapabilityKind::ColorTemperature => NumericUnit::Kelvin,
        CapabilityKind::Temperature => NumericUnit::Celsius,
        CapabilityKind::Humidity => NumericUnit::Percent,
    };
    let Some(range) = numeric_range(wire, unit) else {
        return;
    };
    let (core_range, value_codec) = match codec {
        ValueCodecKind::Percent => (
            NumericRange {
                minimum: 0.,
                maximum: 100.,
                step: 1.,
                unit: NumericUnit::Percent,
            },
            ValueCodec::Percent {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ),
        ValueCodecKind::Identity => (
            range,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ),
    };
    feature.capabilities.0.push(capability.value(core_range));
    feature
        .properties
        .push(mapping(service.iid, wire, property, value_codec.clone()));
    let kind = match property {
        Property::Brightness => CommandKind::Brightness,
        Property::ColorTemperature => CommandKind::ColorTemperature,
        _ => return,
    };
    feature
        .commands
        .push(command(service.iid, wire, kind, value_codec));
}
fn mapping(
    siid: u32,
    property: &PropertySpec<'_>,
    core: Property,
    codec: ValueCodec,
) -> PropertyMapping {
    PropertyMapping {
        property: core,
        siid,
        piid: property.iid,
        readable: property.access.contains(&"read"),
        notify: property.access.contains(&"notify"),
        class: if core == Property::Battery {
            PropertyClass::Ancillary
        } else {
            PropertyClass::Core
        },
        codec,
    }
}
fn command(
    siid: u32,
    property: &PropertySpec<'_>,
    kind: CommandKind,
    codec: ValueCodec,
) -> CommandMapping {
    CommandMapping {
        command: kind,
        target: WireTarget::Property {
            siid,
            piid: property.iid,
        },
        codec,
    }
}
fn action_command(siid: u32, aiid: u32, kind: CommandKind) -> CommandMapping {
    CommandMapping {
        command: kind,
        target: WireTarget::Action { siid, aiid },
        codec: ValueCodec::Identity,
    }
}

fn writable_property<'a>(service: &'a Service<'a>, kind: &str) -> Option<&'a PropertySpec<'a>> {
    service
        .properties
        .iter()
        .find(|property| property.kind == kind && property.access.contains(&"write"))
}
fn readable_property<'a>(service: &'a Service<'a>, kind: &str) -> Option<&'a PropertySpec<'a>> {
    service.properties.iter().find(|property| {
        property.kind == kind
            && (property.access.contains(&"read") || property.access.contains(&"notify"))
    })
}
fn numeric_range(property: &PropertySpec<'_>, unit: NumericUnit) -> Option<NumericRange> {
    if !numeric_format(property.format) {
        return None;
    }
    let expected = match unit {
        NumericUnit::Percent => "percentage",
        NumericUnit::Celsius => "celsius",
        NumericUnit::Kelvin => "kelvin",
        NumericUnit::Lux => "lux",
    };
    if property.unit != Some(expected) {
        return None;
    }
    let (minimum, maximum, step) = property.range?;
    (step > 0. && minimum <= maximum).then_some(NumericRange {
        minimum,
        maximum,
        step,
        unit,
    })
}

fn temperature_command_codec(property: &PropertySpec<'_>, range: NumericRange) -> ValueCodec {
    if property.format == "float" {
        ValueCodec::NumberRange {
            minimum: range.minimum,
            maximum: range.maximum,
            step: range.step,
        }
    } else {
        ValueCodec::IntegerRange {
            minimum: range.minimum,
            maximum: range.maximum,
            step: range.step,
        }
    }
}
fn enum_values(property: &PropertySpec<'_>, names: &[&str]) -> Vec<i64> {
    property
        .values
        .iter()
        .filter(|(_, name)| {
            names
                .iter()
                .any(|expected| name.to_ascii_lowercase() == *expected)
        })
        .map(|(value, _)| *value)
        .collect()
}
fn enum_value(property: &PropertySpec<'_>, names: &[&str]) -> Option<i64> {
    enum_values(property, names).into_iter().next()
}
fn enum_map<T: Copy>(values: &[(i64, &str)], convert: fn(&str) -> Option<T>) -> Vec<(i64, T)> {
    values
        .iter()
        .filter_map(|(value, name)| convert(name).map(|mapped| (*value, mapped)))
        .collect()
}
fn fan_levels(property: &PropertySpec<'_>) -> Vec<(i64, u16)> {
    let mut next = 1;
    property
        .values
        .iter()
        .map(|(raw, name)| {
            let lower = name.to_ascii_lowercase();
            let core = if lower.contains("auto") {
                0
            } else if let Some(level) = lower
                .strip_prefix("level")
                .and_then(|value| value.parse().ok())
            {
                level
            } else {
                let value = next;
                next += 1;
                value
            };
            (*raw, core)
        })
        .collect()
}
fn integer_format(format: &str) -> bool {
    matches!(
        format,
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64"
    )
}
fn hvac_mode(value: &str) -> Option<HvacMode> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Some(HvacMode::Auto),
        "cool" => Some(HvacMode::Cool),
        "heat" => Some(HvacMode::Heat),
        "dry" => Some(HvacMode::Dry),
        "fan" | "wind" => Some(HvacMode::FanOnly),
        "off" => Some(HvacMode::Off),
        _ => None,
    }
}
fn vacuum_mode(value: &str) -> Option<VacuumCleanMode> {
    let v = value.to_ascii_lowercase().replace([' ', '-'], "");
    match v.as_str() {
        "sweep" | "vacuum" => Some(VacuumCleanMode::Vacuum),
        "mop" => Some(VacuumCleanMode::Mop),
        "sweepandmop" | "vacuumandmop" => Some(VacuumCleanMode::VacuumAndMop),
        _ => None,
    }
}
fn vacuum_state(value: &str) -> Option<VacuumOperationalState> {
    let value = value.to_ascii_lowercase();
    if value.contains("charged") || value.contains("docked") {
        Some(VacuumOperationalState::Docked)
    } else if value.contains("return") || value.contains("go charging") {
        Some(VacuumOperationalState::Returning)
    } else if value.contains("charg") {
        Some(VacuumOperationalState::Charging)
    } else if value.contains("sweep") || value.contains("clean") || value.contains("mopping") {
        Some(VacuumOperationalState::Cleaning)
    } else if value.contains("pause") {
        Some(VacuumOperationalState::Paused)
    } else if value.contains("error") || value.contains("fault") {
        Some(VacuumOperationalState::Error)
    } else if value.contains("idle") || value.contains("sleep") {
        Some(VacuumOperationalState::Idle)
    } else {
        None
    }
}
fn event_argument_mappings(
    service: &Service<'_>,
    event: &ActionSpec<'_>,
    class: PropertyClass,
) -> Vec<EventArgumentMapping> {
    // MIoT event arguments refer to property IIDs in their own service. They may
    // have no standalone access, so only this event mapping consumes them.
    event
        .arguments
        .iter()
        .enumerate()
        .filter_map(|(index, iid)| {
            service
                .properties
                .iter()
                .find(|property| property.iid == *iid)
                .map(|property| (index, property))
        })
        .filter_map(|(index, property)| {
            let (core, codec) = match property.kind {
                "illumination" if property.unit == Some("lux") => {
                    let range = numeric_range(property, NumericUnit::Lux)?;
                    (
                        Property::Illuminance,
                        ValueCodec::NumberRange {
                            minimum: range.minimum,
                            maximum: range.maximum,
                            step: range.step,
                        },
                    )
                }
                "occupancy-status" => (
                    Property::Occupancy,
                    ValueCodec::Occupancy {
                        vacant: enum_values(property, &["no one", "noshow", "vacant"]),
                        occupied: enum_values(property, &["has one", "show", "occupied"]),
                    },
                ),
                _ => return None,
            };
            Some(EventArgumentMapping {
                index,
                mapping: PropertyMapping {
                    property: core,
                    siid: service.iid,
                    piid: property.iid,
                    readable: false,
                    notify: false,
                    class,
                    codec,
                },
            })
        })
        .collect()
}

fn sensing_modalities(model: &str) -> Vec<SensingModality> {
    match model {
        "xiaomi.motion.pir1" => vec![SensingModality::Pir],
        "linp.sensor_occupy.hb01" | "izq.sensor_occupy.trio" => {
            vec![SensingModality::Radar]
        }
        "xiaomi.sensor_occupy.03" | "xiaomi.sensor_occupy.p1" => {
            vec![SensingModality::Pir, SensingModality::Radar]
        }
        // Generic models remain explicitly unclassified. Matter represents
        // this protocol-neutral value using its Other modality feature.
        _ => vec![SensingModality::Unspecified],
    }
}
