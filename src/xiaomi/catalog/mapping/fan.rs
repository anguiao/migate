use super::*;

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
            feature
                .definition
                .capabilities
                .0
                .push(Capability::FanSpeeds(
                    values.iter().map(|(_, value)| *value).collect(),
                ));
            feature.binding.properties.push(mapping(
                service.iid,
                property,
                Property::FanSpeed,
                ValueCodec::FanSpeed(values.clone()),
            ));
            feature.binding.commands.push(command(
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
            .definition
            .capabilities
            .0
            .push(Capability::SwingModes(vec![SwingMode::Off, axis]));
        feature.binding.properties.push(mapping(
            service.iid,
            property,
            Property::Oscillation,
            ValueCodec::Bool,
        ));
        feature.binding.commands.push(command(
            service.iid,
            property,
            CommandKind::Oscillation,
            ValueCodec::Bool,
        ));
    }
    output.push(feature);
}
