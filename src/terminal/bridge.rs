mod context;
pub(crate) use context::Context;
use context::View;

use super::{AuthStatus, current_time, status::timestamp};
use crate::{
    RuntimeError,
    configuration::GroupRelation,
    device::{Availability, Capability, Command, DeviceKey, LightMode, Power},
    storage::StorageError,
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID FUNCTION, off DID FUNCTION, help\nUse list to find stable function keys.";
fn invalid(view: &View, status: &AuthStatus, now: i64, reason: &str) -> String {
    format!(
        "{reason}\n{COMMAND_HELP}\n{}",
        view.render_status(status, now)
    )
}
fn configuration_lines(view: &View, did: &str) -> Vec<String> {
    let current = &view.current;
    let key = DeviceKey::new("xiaomi", did);
    let mut lines = Vec::new();
    if view.accessible_descriptions.contains(&key)
        && let Some(source) = current
            .configuration
            .descriptions
            .iter()
            .find(|source| source.key == key)
        && let Some(diagnostic) = &source.diagnostic
    {
        lines.push(format!(
            "  Source {} ({}): recognition issue: {diagnostic}.",
            source.model, source.instance_type
        ));
    }
    if let Some(record) = current.configuration.device(&key) {
        lines.push(format!(
            "  Matter: {}; endpoint {}; configuration version {}.",
            if current.derived.published.contains(&key) {
                "published"
            } else {
                "hidden or reserved"
            },
            record.endpoint,
            record.configuration_version
        ));
        if let Some(native) = &record.native {
            let device = native.device();
            let recognition = device.recognition();
            let category = recognition
                .map(|metadata| metadata.category())
                .unwrap_or_else(|| {
                    native
                        .instance_type()
                        .split(':')
                        .nth(3)
                        .expect("a native instance has a validated URN")
                });
            lines.push(format!("  Category: {category}."));
            for function in device.functions() {
                let role = if recognition
                    .is_some_and(|metadata| metadata.core().iter().any(|id| id == function.id()))
                {
                    "core"
                } else {
                    "additional"
                };
                let mut bindings = function
                    .bindings()
                    .iter()
                    .map(|binding| format!("property {}.{}", binding.siid, binding.piid))
                    .collect::<Vec<_>>();
                if let Some(motion) = function.motion() {
                    let event = motion.event();
                    bindings.push(format!("event {}.{}", event.siid, event.eiid));
                    bindings.extend(motion.arguments().iter().map(|argument| {
                        format!(
                            "event argument {}.{}",
                            argument.property.siid, argument.property.piid
                        )
                    }));
                }
                let bindings = bindings.join(", ");
                lines.push(format!(
                    "  {}: {role}; capabilities {}; bindings {bindings}.",
                    function.id(),
                    capabilities(&function.description())
                ));
            }
            if let Some(battery) = device.battery() {
                let binding = battery.property();
                lines.push(format!(
                    "  Shared battery binding: property {}.{}.",
                    binding.siid, binding.piid
                ));
            }
            if let Some(binding) = device.refresh_trigger() {
                lines.push(format!(
                    "  Refresh trigger: property {}.{}.",
                    binding.siid, binding.piid
                ));
            }
            if let Some(metadata) = recognition {
                for patch in metadata.patches() {
                    lines.push(format!(
                        "  Patch {}: {} ({}).",
                        patch.id(),
                        patch.target(),
                        patch.stage()
                    ));
                }
                for issue in metadata.diagnostics() {
                    lines.push(format!(
                        "  Recognition issue {}: {}.",
                        issue.target(),
                        issue.reason()
                    ));
                }
            }
        }
    }
    // The terminal lists every confirmed membership from the effective
    // relations, including groups that are currently unpublished.
    for relation in current
        .derived
        .groups
        .values()
        .filter(|relation| relation.members.iter().any(|member| member == did))
    {
        let group_key = DeviceKey::new("xiaomi", &relation.group_did);
        let state = if current.derived.published.contains(&group_key) {
            "published".to_string()
        } else {
            format!("not published ({})", unpublished_reason(relation))
        };
        lines.push(format!(
            "  Group {} ({}): {state}.",
            relation.group_did,
            if relation.is_physical {
                "physical"
            } else {
                "virtual"
            }
        ));
    }
    if let Some(group) = current.derived.groups.get(did) {
        lines.push(format!(
            "  Native group ({}): members {}.",
            if group.is_physical {
                "physical"
            } else {
                "virtual"
            },
            group.members.join(", ")
        ));
        if let Some(entry) = current
            .entries
            .iter()
            .find(|entry| entry.physical.key() == &key)
            && let Some(native_function) = entry.native.device().unique_light()
            && let Some(logical_function) = entry.physical.function(native_function.id())
        {
            let native = native_function.description();
            let logical = logical_function.description();
            let repaired = logical.supports_rgb() && !native.supports_rgb();
            lines.push(format!(
                "  Native capabilities: {}. Repaired: {}. State source: {}.",
                capabilities(&native),
                if repaired {
                    "RGB and color mode"
                } else {
                    "none"
                },
                if !group.is_physical {
                    "members: any on, mean brightness, representative color"
                } else if repaired {
                    "native group; RGB/mode from member representative"
                } else {
                    "native group"
                }
            ));
            if let Some(served) = current.derived.served.get(&key)
                && served.repair.is_none()
                && let Some(reason) = &served.repair_reason
            {
                lines.push(format!("  RGB repair: unavailable ({reason})."));
            }
            if let Some(interval) = logical.temperature_interval() {
                lines.push(format!(
                    "  Effective temperature range: {}–{} K.",
                    interval.min(),
                    interval.max()
                ));
            }
        } else {
            lines.push(format!("  Not published: {}.", unpublished_reason(group)));
        }
        if let Some(issues) = current.derived.member_issues.get(did) {
            for issue in issues {
                lines.push(format!("  Member relation: {issue}."));
            }
        }
        if let Some(error) = &group.error {
            lines.push(format!("  Relation: {error}."));
        }
    }
    lines
}

fn capabilities(description: &crate::device::FunctionDescription) -> String {
    description
        .capabilities()
        .iter()
        .map(|capability| match capability {
            Capability::OnOff => "on/off".into(),
            Capability::Curtain => "curtain".into(),
            Capability::Fan => "fan".into(),
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
        let diagnostic = super::status::property_diagnostics(
            properties.iter().find(|property| property.name == name),
        );
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

fn render(view: &View, did: Option<&str>, status: &AuthStatus, show_commands: bool) -> String {
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

fn measurement_text(measurement: crate::device::MeasurementSnapshot, unit: &str) -> String {
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

impl View {
    async fn handle_line(
        &self,
        line: &str,
        now: i64,
        status: &AuthStatus,
    ) -> Result<Option<String>, StorageError> {
        let view = self;
        let registry = self;
        let words = line.split_whitespace().collect::<Vec<_>>();
        let text = match words.as_slice() {
            [] => return Ok(None),
            ["help"] => format!("{COMMAND_HELP}\n{}", view.render_status(status, now)),
            ["list"] => {
                let mut text = render(registry, None, status, true);
                for pending in status.pending() {
                    let key = DeviceKey::new("xiaomi", &pending.did);
                    let name = registry
                        .get(&key)
                        .map(|physical| physical.label())
                        .unwrap_or_else(|| pending.name.clone());
                    text.push_str(&format!(
                        "\n{} ({}): pending ({})",
                        pending.did, name, pending.reason
                    ));
                }
                text
            }
            ["status"] => format!(
                "{}\n{}",
                render(registry, None, status, false),
                view.render_devices(status, now, None)
            ),
            ["status", did] => {
                if registry.get(&DeviceKey::new("xiaomi", *did)).is_none() {
                    let details = configuration_lines(registry, did);
                    if details.is_empty() {
                        invalid(registry, status, now, "Unknown device DID.")
                    } else {
                        format!(
                            "{did}: not published\n{}\n{}",
                            details.join("\n"),
                            view.render_devices(status, now, Some(did))
                        )
                    }
                } else {
                    format!(
                        "{}\n{}",
                        render(registry, Some(did), status, false),
                        view.render_devices(status, now, Some(did))
                    )
                }
            }
            [verb @ ("on" | "off"), did, id] => {
                let Some(physical) = registry.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Ok(Some(invalid(registry, status, now, "Unknown device DID.")));
                };
                let Some(function) = physical.function(id) else {
                    return Ok(Some(invalid(registry, status, now, "Unknown function.")));
                };
                let target = if physical.functions().len() == 1 {
                    did.to_string()
                } else {
                    format!("{did}/{}", function.function_key().id)
                };
                match function
                    .execute(if *verb == "on" {
                        Command::On
                    } else {
                        Command::Off
                    })
                    .await
                {
                    Ok(_) => {
                        log::info!("terminal {target} {verb}: accepted");
                        format!("{target}: command accepted")
                    }
                    Err(error) => format!("{target}: {error}"),
                }
            }
            _ => invalid(registry, status, now, "Invalid command usage."),
        };
        Ok(Some(text))
    }
}

impl Context<'_> {
    pub(crate) async fn handle_line(
        &self,
        line: &str,
        now: i64,
    ) -> Result<Option<String>, StorageError> {
        self.view().handle_line(line, now, self.status).await
    }
}

impl Context<'_> {
    pub(crate) async fn run_input(
        &self,
        input: impl futures_lite::io::AsyncBufRead + Unpin,
        output: impl futures_lite::io::AsyncWrite + Unpin,
    ) -> Result<(), RuntimeError> {
        read_commands(input, output, async |line| {
            self.handle_line(line, current_time()).await
        })
        .await
    }
}

/// Own terminal I/O while each command obtains its current committed view.
async fn read_commands(
    mut input: impl futures_lite::io::AsyncBufRead + Unpin,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
    mut handle: impl AsyncFnMut(&str) -> Result<Option<String>, StorageError>,
) -> Result<(), RuntimeError> {
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(result) = handle(&line).await? {
            output.write_all(result.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
