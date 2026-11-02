mod configuration;

use super::{AuthStatus, View};
use crate::{
    configuration::GroupRelation,
    device::{AmbientTemperature, Availability, Capability, DeviceKey, LightMode, Power},
    terminal::status::timestamp,
};
pub(super) use configuration::configuration_lines;

fn capabilities(description: &crate::device::FunctionDescription) -> String {
    description
        .capabilities()
        .iter()
        .map(|capability| match capability {
            Capability::OnOff => "on/off".into(),
            Capability::Curtain => "curtain".into(),
            Capability::Fan => "fan".into(),
            Capability::AirConditioner => "air conditioner".into(),
            Capability::Brightness => "brightness".into(),
            Capability::ColorTemperature(interval) => {
                format!("temperature {}–{} K", interval.min(), interval.max())
            }
            Capability::ColorRgb => "RGB".into(),
            Capability::Temperature => "temperature".into(),
            Capability::Humidity => "humidity".into(),
            Capability::CarbonDioxide => "co2".into(),
            Capability::Illuminance => "illuminance".into(),
            Capability::Contact => "contact".into(),
            Capability::Motion => "motion".into(),
            Capability::Occupancy => "occupancy".into(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn unpublished_reason(relation: &GroupRelation) -> String {
    if let Some(error) = &relation.error {
        return error.clone();
    }
    if !relation.active {
        return "confirmed empty relation".into();
    }
    if relation.instance_type.is_none() {
        return "native group description has no instance type".into();
    }
    "waiting for a usable native description".into()
}

fn observed_function_lines(
    status: &AuthStatus,
    did: &str,
    function: &crate::device::Function,
    entries: impl IntoIterator<Item = (&'static str, String, Option<i64>, Option<i64>)>,
    show_commands: bool,
) -> Vec<String> {
    let id = &function.function_key().id;
    let label = &function.description().label;
    let properties = status.function_properties(did, id);
    let mut lines = vec![format!("  {id} ({label}):")];
    for (name, value, source_time, received_at) in entries {
        let source_time = source_time.map_or_else(|| "unknown".into(), |time| time.to_string());
        let received_at = received_at.map_or_else(|| "unknown".into(), timestamp);
        let diagnostic =
            crate::terminal::status::property_diagnostics(properties.iter().find(|property| {
                property.name
                    == if name == "Target temperature" {
                        "Temperature"
                    } else {
                        name
                    }
            }));
        lines.push(format!("    {name}: {value}; upstream time {source_time}; received at {received_at}; {diagnostic}."));
    }
    if show_commands {
        lines.extend(
            status
                .function_commands(did, id)
                .into_iter()
                .map(|command| format!("    {command}")),
        );
    }
    lines
}

pub(super) fn render(
    view: &View,
    did: Option<&str>,
    status: &AuthStatus,
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
            unpublished_reason(relation)
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
            lines.push(format!(
                "  battery (shared): {}",
                measurement_text(
                    crate::device::MeasurementSnapshot {
                        value: battery.value,
                        source_time: battery.source_time,
                        received_at: battery.received_at,
                    },
                    "%"
                )
            ));
        }
        for function in physical.functions() {
            let id = &function.function_key().id;
            let label = &function.description().label;
            let snapshot = function.snapshot();
            match snapshot.state {
                crate::device::FunctionState::AirConditioner {
                    air_conditioner: ac,
                } => {
                    let text = |value: Option<bool>| {
                        value.map_or_else(
                            || "unknown".into(),
                            |value| if value { "on" } else { "off" }.into(),
                        )
                    };
                    let mut entries = vec![
                        (
                            "Power",
                            text(ac.power.value),
                            ac.power.source_time,
                            ac.power.received_at,
                        ),
                        (
                            "Mode",
                            ac.mode
                                .value
                                .map_or_else(|| "unknown".into(), |value| format!("{value:?}")),
                            ac.mode.source_time,
                            ac.mode.received_at,
                        ),
                        (
                            "Target temperature",
                            ac.temperature
                                .value
                                .map_or_else(|| "unknown".into(), |value| format!("{value} °C")),
                            ac.temperature.source_time,
                            ac.temperature.received_at,
                        ),
                        (
                            "Speed",
                            ac.speed
                                .value
                                .map_or_else(|| "unknown".into(), |value| format!("{value:?}")),
                            ac.speed.source_time,
                            ac.speed.received_at,
                        ),
                    ];
                    if matches!(
                        function.description().kind,
                        crate::device::FunctionKind::AirConditioner {
                            vertical_swing: true,
                            ..
                        }
                    ) {
                        entries.push((
                            "Vertical swing",
                            text(ac.swing.value),
                            ac.swing.source_time,
                            ac.swing.received_at,
                        ));
                    }
                    lines.extend(observed_function_lines(
                        status,
                        &key.id,
                        function,
                        entries,
                        show_commands,
                    ));
                    let ambient = snapshot
                        .ambient_temperature()
                        .expect("air-conditioner projection");
                    let source = view
                        .current
                        .configuration
                        .temperature_bindings
                        .get(function.function_key());
                    if let Some(source) = source {
                        let device = view.get(&source.device);
                        let name = device.as_ref().map(|device| device.label());
                        lines.push(format!(
                            "    Ambient source: {}/{}{}.",
                            source.device.id,
                            source.id,
                            name.map(|name| format!(" ({name})")).unwrap_or_default()
                        ));
                        let text = match ambient {
                            AmbientTemperature::Available(reading) => measurement_text(reading, "°C"),
                            AmbientTemperature::SourceMissing => "unknown (source not in current directory or access revoked); upstream time unknown; received at unknown".into(),
                            AmbientTemperature::SourceUnavailable => "unknown (temperature source unavailable); upstream time unknown; received at unknown".into(),
                            AmbientTemperature::AwaitingReading => "unknown (awaiting a valid temperature reading); upstream time unknown; received at unknown".into(),
                            AmbientTemperature::Unbound => unreachable!("bound source has an ambient projection"),
                        };
                        lines.push(format!("    Ambient temperature: {text}."));
                        if !matches!(ambient, AmbientTemperature::Available(_))
                            && let Some(sensor) =
                                device.and_then(|device| device.function(&source.id))
                            && let Some(reading) = sensor.snapshot().measurement()
                            && reading.value.is_some()
                        {
                            lines.push(format!(
                                "    Last reading (diagnostic only): {}.",
                                measurement_text(reading, "°C")
                            ));
                        }
                        lines.push(format!(
                            "    Ambient source synchronization: {}.",
                            status.function_sync(&source.device.id, &source.id)
                        ));
                    } else {
                        lines.push("    Ambient temperature: unknown (not bound); upstream time unknown; received at unknown.".into());
                    }
                    if ac.power.value == Some(true)
                        && ac.speed.value == Some(crate::device::AirConditionerFanSpeed::Auto)
                    {
                        lines.push("    Current fan level: unknown (automatic speed).".into());
                    }
                    if let Some(property) = status
                        .function_properties(&key.id, &function.function_key().id)
                        .iter()
                        .find(|property| property.name == "Control range")
                    {
                        lines.push(format!(
                            "    Control range source: {}.",
                            crate::terminal::status::property_diagnostics(Some(property))
                        ));
                    }
                    if !function.air_conditioner_constraints_ready() {
                        lines.push("    Control range is awaiting source confirmation.".into());
                    }
                    if let Some(constraints) = function.air_conditioner_constraints() {
                        for mode in crate::device::air_conditioner::AirConditionerMode::ALL {
                            if let Some(temperatures) = constraints.temperatures(mode) {
                                lines.push(format!(
                                    "    {mode:?}: temperatures {temperatures:?} °C; speeds {:?}.",
                                    constraints.speeds(mode).unwrap()
                                ));
                            }
                        }
                    } else {
                        lines.push("    Control range: unknown.".into());
                    }
                    let pending = function.pending_air_conditioner_commands();
                    if !pending.is_empty() {
                        lines.push(format!("    queued air-conditioner commands: {pending:?}."));
                    }
                }
                crate::device::FunctionState::Fan { fan } => {
                    let mut entries = vec![
                        (
                            "Power",
                            fan.power.value.map_or_else(
                                || "unknown".into(),
                                |v| if v { "on" } else { "off" }.into(),
                            ),
                            fan.power.source_time,
                            fan.power.received_at,
                        ),
                        (
                            "Speed",
                            fan.speed.value.map_or_else(
                                || "unknown".into(),
                                |v| {
                                    let crate::device::FunctionKind::Fan { speed_count, .. } =
                                        function.description().kind
                                    else {
                                        unreachable!()
                                    };
                                    format!("level {v}/{speed_count}")
                                },
                            ),
                            fan.speed.source_time,
                            fan.speed.received_at,
                        ),
                    ];
                    if let Some(mode) = status
                        .function_properties(&key.id, id)
                        .iter()
                        .find_map(|property| property.native_mode)
                    {
                        entries.insert(
                            1,
                            (
                                "Native mode",
                                mode.value
                                    .map_or_else(|| "unknown".into(), |v| v.to_string()),
                                mode.source_time,
                                mode.received_at,
                            ),
                        );
                    }
                    if matches!(
                        function.description().kind,
                        crate::device::FunctionKind::Fan {
                            horizontal_swing: true,
                            ..
                        }
                    ) {
                        entries.push((
                            "Horizontal swing",
                            fan.swing.value.map_or_else(
                                || "unknown".into(),
                                |v| if v { "on" } else { "off" }.into(),
                            ),
                            fan.swing.source_time,
                            fan.swing.received_at,
                        ));
                    }
                    lines.extend(observed_function_lines(
                        status,
                        &key.id,
                        function,
                        entries,
                        show_commands,
                    ));
                    let pending = function.pending_fan_commands();
                    if !pending.is_empty() {
                        let pending = pending
                            .into_iter()
                            .map(|command| {
                                crate::xiaomi::adapter::CommandTarget::Fan(command).to_string()
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        lines.push(format!("    queued fan commands: {pending}."));
                    }
                }
                crate::device::FunctionState::Curtain { curtain } => {
                    let entries = [
                        (
                            "Current position",
                            curtain
                                .current_position
                                .value
                                .map_or_else(|| "unknown".into(), |value| format!("{value}%")),
                            curtain.current_position.source_time,
                            curtain.current_position.received_at,
                        ),
                        (
                            "Target position",
                            curtain
                                .target_position
                                .value
                                .map_or_else(|| "unknown".into(), |value| format!("{value}%")),
                            curtain.target_position.source_time,
                            curtain.target_position.received_at,
                        ),
                        (
                            "Status",
                            match curtain.status.value {
                                None => "unknown",
                                Some(crate::device::CurtainMotion::Opening) => "opening",
                                Some(crate::device::CurtainMotion::Closing) => "closing",
                                Some(crate::device::CurtainMotion::Stopped) => "stopped",
                            }
                            .into(),
                            curtain.status.source_time,
                            curtain.status.received_at,
                        ),
                    ];
                    lines.extend(observed_function_lines(
                        status,
                        &key.id,
                        function,
                        entries,
                        show_commands,
                    ));
                    let pending = function.pending_curtain_commands();
                    if !pending.is_empty() {
                        let pending = pending
                            .into_iter()
                            .map(|command| {
                                crate::xiaomi::adapter::CommandTarget::Curtain(command).to_string()
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        lines.push(format!("    queued curtain commands: {pending}."));
                    }
                }
                crate::device::FunctionState::Outlet { power } => {
                    lines.push(format!("  {id} ({label}): {}", power_text(power)));
                }
                crate::device::FunctionState::Light { power, lighting } => {
                    lines.push(format!("  {id} ({label}): {}", power_text(power)));
                    let description = function.description();
                    let capabilities = capabilities(description);
                    lines.push(format!("    Capabilities: {capabilities}."));
                    let mut values = Vec::new();
                    if description.supports_brightness() {
                        values.push(format!(
                            "Brightness: {}",
                            lighting
                                .brightness_percent
                                .map_or_else(|| "unknown".into(), |value| format!("{value}%"))
                        ));
                    }
                    if description.supports_color_temperature() {
                        values.push(format!(
                            "temperature: {}",
                            lighting
                                .temperature_kelvin
                                .map_or_else(|| "unknown".into(), |value| format!("{value} K"))
                        ));
                    }
                    if description.supports_rgb() {
                        values.push(format!(
                            "RGB: {}",
                            lighting.color.map_or_else(
                                || "unknown".into(),
                                |value| format!(
                                    "#{:02X}{:02X}{:02X}",
                                    value.red, value.green, value.blue
                                )
                            )
                        ));
                    }
                    if description.supports_color_temperature() || description.supports_rgb() {
                        values.push(format!(
                            "mode: {}",
                            match lighting.mode {
                                None => "unknown",
                                Some(LightMode::Temperature) => "temperature",
                                Some(LightMode::Rgb) => "RGB",
                            }
                        ));
                    }
                    if !values.is_empty() {
                        lines.push(format!("    {}.", values.join("; ")));
                    }
                }
                crate::device::FunctionState::Temperature { measurement } => {
                    lines.push(format!(
                        "  {id} ({label}): {}",
                        measurement_text(measurement, "°C")
                    ));
                }
                crate::device::FunctionState::Humidity { measurement } => {
                    lines.push(format!(
                        "  {id} ({label}): {}",
                        measurement_text(measurement, "%")
                    ));
                }
                crate::device::FunctionState::CarbonDioxide { measurement } => {
                    lines.push(format!(
                        "  {id} ({label}): {}",
                        measurement_text(measurement, "ppm")
                    ));
                }
                crate::device::FunctionState::Contact { state }
                | crate::device::FunctionState::Motion { state }
                | crate::device::FunctionState::Occupancy { state } => {
                    lines.push(format!(
                        "  {id} ({label}): {}",
                        state_text(function.description().kind, state)
                    ));
                }
                crate::device::FunctionState::Illuminance { measurement } => {
                    lines.push(format!(
                        "  {id} ({label}): {}",
                        measurement_text(measurement, "lux")
                    ));
                }
            }
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

fn power_text(power: Power) -> &'static str {
    match power {
        Power::Unknown => "unknown",
        Power::On => "on",
        Power::Off => "off",
    }
}

fn state_text(kind: crate::device::FunctionKind, state: crate::device::StateSnapshot) -> String {
    use crate::device::{FunctionKind, StateInput};
    let value = match (kind, state.active) {
        (_, None) => "unknown",
        (FunctionKind::Contact, Some(true)) => "open",
        (FunctionKind::Contact, Some(false)) => "closed",
        (FunctionKind::Motion, Some(true)) => "motion detected",
        (FunctionKind::Motion, Some(false)) => "no motion",
        (FunctionKind::Occupancy, Some(true)) => "occupied",
        (FunctionKind::Occupancy, Some(false)) => "unoccupied",
        _ => unreachable!("state text is only used for state functions"),
    };
    let input = match state.input {
        None => "native input unknown".into(),
        Some(StateInput::Property(value)) => format!("native property {value}"),
        Some(StateInput::Event { siid, eiid }) => format!("native event {siid}.{eiid}"),
    };
    let source = state
        .source_time
        .map_or_else(|| "unknown".into(), |value| value.to_string());
    let received = state
        .received_at
        .map_or_else(|| "unknown".into(), timestamp);
    format!("{value}; {input}; upstream time {source}; received at {received}")
}

pub(super) fn measurement_text(
    measurement: crate::device::MeasurementSnapshot,
    unit: &str,
) -> String {
    let value = measurement
        .value
        .map_or_else(|| "unknown".into(), |value| format!("{value:.2} {unit}"));
    let source = measurement
        .source_time
        .map_or_else(|| "unknown".into(), |value| value.to_string());
    let received = measurement
        .received_at
        .map_or_else(|| "unknown".into(), timestamp);
    format!("{value}; upstream time {source}; received at {received}")
}
