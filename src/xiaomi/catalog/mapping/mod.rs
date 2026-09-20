mod climate;
mod curtain;
mod fan;
mod lighting;
mod sensors;
mod vacuum;

use climate::*;
use curtain::*;
use fan::*;
use lighting::*;
use sensors::*;
use vacuum::*;

use super::binding::*;
use super::codec::{CompileError, ValueCodec};
use super::spec::{ActionSpec, PropertySpec, Service, numeric_format};
use crate::device::{
    Capability, FeatureCapabilities, FeatureDefinition, FeatureRole, HvacMode, NumericRange,
    NumericUnit, Property, SensingModality, SwingMode, VacuumCleanMode, VacuumOperationalState,
};

pub(super) fn compile_features(
    model: &str,
    device_type: &str,
    parsed: &[Service<'_>],
) -> Result<Vec<FeatureDescriptor>, CompileError> {
    let mut features = Vec::new();
    match device_type {
        "light" => compile_lights(parsed, FeatureRole::Light, &mut features),
        "switch" | "outlet" | "control-panel" => compile_loads(parsed, &mut features),
        "air-conditioner" | "air-condition-outlet" => compile_climate(model, parsed, &mut features),
        "curtain" => compile_curtain(parsed, &mut features),
        "fan" => compile_fan(parsed, &mut features),
        "temperature-humidity-sensor" => compile_temperature_humidity(parsed, &mut features),
        "motion-sensor" => compile_motion(model, parsed, &mut features)?,
        "occupancy-sensor" => compile_occupancy(model, parsed, &mut features),
        "magnet-sensor" => compile_contact(parsed, &mut features),
        "vacuum" => compile_vacuum(parsed, &mut features),
        "bath-heater" => compile_bath_heater(parsed, &mut features),
        _ => {}
    }
    Ok(features)
}

fn powered_feature(service: &Service<'_>, role: FeatureRole) -> Option<FeatureDescriptor> {
    let power = writable_property(service, "on").filter(|property| property.format == "bool")?;
    let mut feature = base_feature(service, role);
    feature
        .definition
        .capabilities
        .0
        .push(Capability::Power { writable: true });
    feature.binding.properties.push(mapping(
        service.iid,
        power,
        Property::Power,
        ValueCodec::Bool,
    ));
    feature.binding.commands.push(command(
        service.iid,
        power,
        CommandKind::Power,
        ValueCodec::Bool,
    ));
    Some(feature)
}
fn base_feature(service: &Service<'_>, role: FeatureRole) -> FeatureDescriptor {
    FeatureDescriptor {
        definition: FeatureDefinition {
            service_instance: service.iid,
            role,
            name: service.name.to_owned(),
            capabilities: FeatureCapabilities::default(),
        },
        binding: XiaomiBinding::default(),
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
        feature
            .definition
            .capabilities
            .0
            .push(Capability::Battery(range));
        feature.binding.properties.push(mapping(
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
    feature
        .definition
        .capabilities
        .0
        .push(capability.value(core_range));
    feature
        .binding
        .properties
        .push(mapping(service.iid, wire, property, value_codec.clone()));
    let kind = match property {
        Property::Brightness => CommandKind::Brightness,
        Property::ColorTemperature => CommandKind::ColorTemperature,
        _ => return,
    };
    feature
        .binding
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
