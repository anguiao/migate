use super::*;

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
    if range.minimum < 0.0 || range.maximum <= 0.0 {
        return;
    }
    let codec = ValueCodec::Percent {
        minimum: range.minimum,
        maximum: range.maximum,
        step: range.step,
    };
    let mut feature = base_feature(service, FeatureRole::Curtain);
    feature
        .definition
        .capabilities
        .0
        .push(Capability::CurtainPosition(NumericRange {
            minimum: range.minimum * 100. / range.maximum,
            maximum: 100.,
            step: range.step * 100. / range.maximum,
            unit: NumericUnit::Percent,
        }));
    feature.binding.commands.push(command(
        service.iid,
        target,
        CommandKind::CurtainPosition,
        codec.clone(),
    ));
    feature.binding.properties.push(mapping(
        service.iid,
        target,
        Property::CurtainTargetPosition,
        codec.clone(),
    ));
    if let Some(current) = readable_property(service, "current-position")
        && let Some(current_range) = numeric_range(current, NumericUnit::Percent)
    {
        feature.binding.properties.push(mapping(
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
        feature.binding.properties.push(mapping(
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
        feature
            .definition
            .capabilities
            .0
            .push(Capability::CurtainStop);
        feature.binding.commands.push(CommandMapping {
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
