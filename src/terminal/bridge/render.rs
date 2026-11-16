mod configuration;
mod control;
mod diagnostics;
mod measurement;
mod primitives;

use super::View;
use crate::device::{
    AmbientTemperature, Availability, DeviceKey, Function, FunctionKey, FunctionState,
    MeasurementSnapshot,
};
pub(super) use configuration::configuration_lines;
use diagnostics::FunctionDiagnostics;
use measurement::AmbientSource;

pub(super) fn render(
    view: &View,
    did: Option<&str>,
    status: &crate::xiaomi::adapter::AdapterStatus,
    show_commands: bool,
) -> String {
    let mut lines = Vec::new();
    for source in &view.current.configuration.descriptions {
        if !view.accessible_descriptions.contains(&source.key)
            || did.is_some_and(|did| did != source.key.id)
        {
            continue;
        }
        if let Some(diagnostic) = &source.diagnostic {
            lines.push(format!(
                "{} ({}): not published ({}).",
                source.key.id, source.model, diagnostic
            ));
        }
    }
    for relation in view.current.derived.groups.values() {
        if did.is_some_and(|did| did != relation.group_did) {
            continue;
        }
        let key = DeviceKey::new("xiaomi", &relation.group_did);
        if view.get(&key).is_some() {
            continue;
        }
        lines.push(format!(
            "{} (native group): not published ({}).",
            relation.group_did,
            configuration::unpublished_reason(relation)
        ));
    }
    for physical in view
        .list()
        .into_iter()
        .filter(|physical| did.is_none_or(|did| physical.key().id == did))
    {
        let key = physical.key();
        let availability = match physical.availability() {
            Availability::Initializing => "initializing".into(),
            Availability::Available => "available".into(),
            Availability::Unavailable(reason) => format!("unavailable ({reason})"),
        };
        lines.push(format!("{} ({}): {availability}", key.id, physical.label()));
        lines.extend(configuration_lines(view, &key.id));
        if let Some(battery) = physical.battery_snapshot() {
            lines.push(measurement::battery_line(battery));
        }
        let diagnostic = status.devices.iter().find(|device| device.did == key.id);
        for function in physical.functions() {
            lines.extend(function_lines(
                view,
                function,
                status,
                diagnostic,
                show_commands,
            ));
            if let Some(issue) = crate::matter::measurement_mapping_issue(function) {
                lines.push(format!("    Matter mapping unsupported: {issue}."));
            }
        }
    }
    if lines.is_empty() {
        "No devices are currently connected to the bridge.".into()
    } else {
        lines.join("\n")
    }
}

fn function_lines(
    view: &View,
    function: &Function,
    status: &crate::xiaomi::adapter::AdapterStatus,
    diagnostic: Option<&crate::xiaomi::adapter::DeviceStatus>,
    show_commands: bool,
) -> Vec<String> {
    let key = function.function_key();
    let description = function.description();
    let snapshot = function.snapshot();
    let diagnostics = FunctionDiagnostics::capture(diagnostic, &key.id, show_commands);
    match snapshot.state {
        FunctionState::AirConditioner { air_conditioner } => {
            let ambient = snapshot
                .ambient_temperature()
                .expect("air-conditioner projection");
            let source = ambient_source(view, key, ambient, status);
            control::air_conditioner_lines(
                description,
                air_conditioner,
                ambient,
                source.as_ref(),
                &diagnostics,
                control::AirConditionerControl {
                    constraints_ready: function.air_conditioner_constraints_ready(),
                    constraints: function.air_conditioner_constraints(),
                    pending: function.pending_air_conditioner_commands(),
                },
            )
        }
        FunctionState::Heater { heater } => control::heater_lines(
            description,
            heater,
            &diagnostics,
            function.pending_heater_commands(),
        ),
        FunctionState::Fan { fan } => control::fan_lines(
            description,
            fan,
            &diagnostics,
            function.pending_fan_commands(),
        ),
        FunctionState::Curtain { curtain } => control::curtain_lines(
            description,
            curtain,
            &diagnostics,
            function.pending_curtain_commands(),
        ),
        FunctionState::Outlet { power } => vec![control::power_line(description, power)],
        FunctionState::Relay { power, mode } => configuration::relay_lines(
            view,
            key,
            description,
            power,
            mode,
            &diagnostics,
            show_commands,
        ),
        FunctionState::Light { power, lighting } => {
            control::light_lines(description, power, lighting)
        }
        FunctionState::Temperature { measurement } => {
            vec![measurement::measurement_line(
                description,
                measurement,
                "°C",
            )]
        }
        FunctionState::Humidity { measurement } => {
            vec![measurement::measurement_line(description, measurement, "%")]
        }
        FunctionState::CarbonDioxide { measurement } => {
            vec![measurement::measurement_line(
                description,
                measurement,
                "ppm",
            )]
        }
        FunctionState::Illuminance { measurement } => {
            vec![measurement::measurement_line(
                description,
                measurement,
                "lux",
            )]
        }
        FunctionState::Contact { state }
        | FunctionState::Motion { state }
        | FunctionState::Occupancy { state } => {
            vec![measurement::state_line(description, state)]
        }
    }
}

fn ambient_source(
    view: &View,
    key: &FunctionKey,
    ambient: AmbientTemperature,
    status: &crate::xiaomi::adapter::AdapterStatus,
) -> Option<AmbientSource> {
    let source = view.current.configuration.temperature_bindings.get(key)?;
    let device = view.get(&source.device);
    let last_reading = if matches!(ambient, AmbientTemperature::Available(_)) {
        None
    } else {
        device
            .as_ref()
            .and_then(|device| device.function(&source.id))
            .and_then(|sensor| sensor.snapshot().measurement())
    };
    Some(AmbientSource {
        key: source.clone(),
        label: device.map(|device| device.label()),
        last_reading,
        synchronization: diagnostics::synchronization(
            status
                .devices
                .iter()
                .find(|device| device.did == source.device.id)
                .and_then(|device| {
                    device
                        .functions
                        .iter()
                        .find(|function| function.function == source.id)
                }),
        ),
    })
}

pub(super) fn measurement_text(measurement: MeasurementSnapshot, unit: &str) -> String {
    measurement::text(measurement, unit)
}
