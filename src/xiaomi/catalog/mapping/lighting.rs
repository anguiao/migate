use super::*;

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
                feature.definition.capabilities.0.push(Capability::Color);
                feature.binding.commands.push(command(
                    service.iid,
                    color,
                    CommandKind::Color,
                    codec.clone(),
                ));
                feature.binding.properties.push(mapping(
                    service.iid,
                    color,
                    Property::Color,
                    codec,
                ));
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
