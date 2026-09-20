use super::*;

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
    feature
        .definition
        .capabilities
        .0
        .push(Capability::VacuumControl);
    for (kind, action) in [
        (CommandKind::VacuumStart, start.unwrap()),
        (CommandKind::VacuumStop, stop.unwrap()),
    ] {
        feature
            .binding
            .commands
            .push(action_command(service.iid, action.iid, kind));
    }
    if let Some(battery) = services.iter().find(|item| item.kind == "battery")
        && let Some(action) = battery
            .actions
            .iter()
            .find(|action| action.kind == "start-charge" && action.arguments.is_empty())
    {
        feature
            .definition
            .capabilities
            .0
            .push(Capability::VacuumDock);
        feature.binding.commands.push(action_command(
            battery.iid,
            action.iid,
            CommandKind::VacuumDock,
        ));
    }
    if let Some(mode) = writable_property(service, "mode") {
        let values = enum_map(&mode.values, vacuum_mode);
        if !values.is_empty() {
            feature
                .definition
                .capabilities
                .0
                .push(Capability::VacuumCleanModes(
                    values.iter().map(|(_, v)| *v).collect(),
                ));
            feature.binding.properties.push(mapping(
                service.iid,
                mode,
                Property::VacuumCleanMode,
                ValueCodec::VacuumClean(values.clone()),
            ));
            feature.binding.commands.push(command(
                service.iid,
                mode,
                CommandKind::VacuumCleanMode,
                ValueCodec::VacuumClean(values),
            ));
        }
    }
    if let Some(status) = readable_property(service, "status") {
        feature.binding.properties.push(mapping(
            service.iid,
            status,
            Property::VacuumOperationalState,
            ValueCodec::VacuumState(enum_map(&status.values, vacuum_state)),
        ));
    }
    if let Some(fault) = readable_property(service, "fault").filter(|property| {
        integer_format(property.format)
            && (property.range.is_some_and(|(minimum, maximum, step)| {
                minimum.fract() == 0. && maximum.fract() == 0. && step.fract() == 0.
            }) || !property.values.is_empty())
    }) {
        feature.binding.properties.push(mapping(
            service.iid,
            fault,
            Property::VacuumFault,
            ValueCodec::Fault {
                range: fault.range,
                values: fault.values.iter().map(|(value, _)| *value).collect(),
            },
        ));
    }
    attach_battery(services, &mut feature);
    output.push(feature);
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
