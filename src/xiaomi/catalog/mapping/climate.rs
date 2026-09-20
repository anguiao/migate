use super::*;

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
            .definition
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
        feature.binding.properties.push(mapping(
            service.iid,
            property,
            Property::TargetTemperature,
            ValueCodec::NumberRange {
                minimum: range.minimum,
                maximum: range.maximum,
                step: range.step,
            },
        ));
        feature.binding.commands.push(command(
            service.iid,
            property,
            CommandKind::TargetTemperature,
            codec,
        ));
    }
    if let Some(property) = readable_property(service, "temperature")
        && let Some(range) = numeric_range(property, NumericUnit::Celsius)
    {
        feature
            .definition
            .capabilities
            .0
            .push(Capability::Temperature(range));
        feature.binding.properties.push(mapping(
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
            feature
                .definition
                .capabilities
                .0
                .push(Capability::HvacModes(
                    values.iter().map(|(_, value)| *value).collect(),
                ));
            feature.binding.properties.push(mapping(
                service.iid,
                property,
                Property::HvacMode,
                ValueCodec::Hvac(values.clone()),
            ));
            feature.binding.commands.push(command(
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
        if let Some(property) =
            writable_property(fan, "fan-level").filter(|property| integer_format(property.format))
        {
            let values = fan_levels(property);
            if !values.is_empty() {
                feature
                    .definition
                    .capabilities
                    .0
                    .push(Capability::FanSpeeds(
                        values.iter().map(|(_, value)| *value).collect(),
                    ));
                feature.binding.properties.push(mapping(
                    fan.iid,
                    property,
                    Property::FanSpeed,
                    ValueCodec::FanSpeed(values.clone()),
                ));
                feature.binding.commands.push(command(
                    fan.iid,
                    property,
                    CommandKind::FanSpeed,
                    ValueCodec::FanSpeed(values),
                ));
            }
        }
        if let Some(property) =
            writable_property(fan, "vertical-swing").filter(|property| property.format == "bool")
        {
            feature
                .definition
                .capabilities
                .0
                .push(Capability::SwingModes(vec![
                    SwingMode::Off,
                    SwingMode::Vertical,
                ]));
            feature.binding.properties.push(mapping(
                fan.iid,
                property,
                Property::SwingMode,
                ValueCodec::SwingBool(SwingMode::Vertical),
            ));
            feature.binding.commands.push(command(
                fan.iid,
                property,
                CommandKind::SwingMode,
                ValueCodec::SwingBool(SwingMode::Vertical),
            ));
        }
    }
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
                .definition
                .capabilities
                .0
                .push(Capability::Power { writable: true });
            feature.binding.properties.push(mapping(
                service.iid,
                property,
                Property::Power,
                ValueCodec::Bool,
            ));
            feature.binding.commands.push(command(
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
            .definition
            .capabilities
            .0
            .push(Capability::Power { writable: true });
        feature.binding.properties.push(mapping(
            service.iid,
            heating,
            Property::Power,
            ValueCodec::Bool,
        ));
        feature.binding.commands.push(command(
            service.iid,
            heating,
            CommandKind::Power,
            ValueCodec::Bool,
        ));
        if let Some(target) = writable_property(service, "target-temperature")
            && let Some(range) = numeric_range(target, NumericUnit::Celsius)
        {
            feature
                .definition
                .capabilities
                .0
                .push(Capability::TargetTemperature(range));
            feature.binding.properties.push(mapping(
                service.iid,
                target,
                Property::TargetTemperature,
                ValueCodec::NumberRange {
                    minimum: range.minimum,
                    maximum: range.maximum,
                    step: range.step,
                },
            ));
            feature.binding.commands.push(command(
                service.iid,
                target,
                CommandKind::TargetTemperature,
                temperature_command_codec(target, range),
            ));
        }
        output.push(feature);
    }
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
