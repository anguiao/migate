use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{
        AppliedGroup, Availability, Capability, Category, Command, DeviceKey, DeviceRegistry,
        Function, FunctionSnapshot, GroupConfiguration, GroupDescription, GroupSnapshot, LightMode,
        PhysicalDevice, Power, RegistrySnapshot, Rgb,
    },
    storage::{DeviceStore, StorageError},
    xiaomi::adapter::PendingDevice,
};
use std::collections::HashMap;
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID [channel], off DID [channel], help\nUse list to find stable channel keys. A single-channel device may omit channel; channel is required for a multi-channel device.";
fn invalid(status: &AuthStatus, now: i64, reason: &str) -> String {
    format!("{reason}\n{COMMAND_HELP}\n{}", status.render(now))
}

/// Saved or live names for every key the applied and pending views can name.
/// A member whose source path was revoked keeps its reserved published name.
type Labels = HashMap<DeviceKey, String>;

fn labels(store: &DeviceStore, snapshot: &RegistrySnapshot) -> Result<Labels, StorageError> {
    let mut keys = Vec::new();
    for group in &snapshot.applied.groups {
        keys.extend(group.members.iter().cloned());
    }
    keys.extend(
        snapshot
            .applied
            .devices
            .iter()
            .map(|device| device.key().clone()),
    );
    if let Some(pending) = &snapshot.pending {
        for group in &pending.groups {
            keys.extend(group.members.iter().cloned());
        }
        keys.extend(pending.devices.iter().map(|device| device.key().clone()));
        keys.extend(pending.removed.iter().cloned());
    }
    let mut labels = Labels::new();
    for key in keys {
        if labels.contains_key(&key) {
            continue;
        }
        let label = match snapshot.devices.iter().find(|device| device.key() == &key) {
            Some(device) => device.label(),
            None => match store.load_published(&key)? {
                Some(stored) => stored.label,
                None => key.id.clone(),
            },
        };
        labels.insert(key, label);
    }
    Ok(labels)
}

fn member_name(labels: &Labels, key: &DeviceKey) -> String {
    format!(
        "{} ({})",
        key.id,
        labels.get(key).map(String::as_str).unwrap_or(&key.id)
    )
}

fn member_names<'a>(labels: &Labels, members: impl IntoIterator<Item = &'a DeviceKey>) -> String {
    members
        .into_iter()
        .map(|member| member_name(labels, member))
        .collect::<Vec<_>>()
        .join(", ")
}

fn capability_list(capabilities: &[Capability]) -> String {
    if capabilities.is_empty() {
        return "none".into();
    }
    capabilities
        .iter()
        .map(|capability| match capability {
            Capability::OnOff => "on/off".into(),
            Capability::Brightness => "brightness".into(),
            Capability::ColorTemperature(range) => {
                format!("temperature {}–{} K", range.min, range.effective_max())
            }
            Capability::ColorRgb => "RGB".into(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn availability_text(availability: &Availability) -> String {
    match availability {
        Availability::Initializing => "initializing".into(),
        Availability::Available => "available".into(),
        Availability::Unavailable(reason) => format!("unavailable ({reason})"),
    }
}

fn power_text(power: Power) -> &'static str {
    match power {
        Power::Unknown => "unknown",
        Power::On => "on",
        Power::Off => "off",
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| format!("{value}%"))
}

fn kelvin(value: Option<u32>) -> String {
    value.map_or_else(|| "unknown".into(), |value| format!("{value} K"))
}

fn color(value: Option<Rgb>) -> String {
    value.map_or_else(
        || "unknown".into(),
        |value| format!("#{:02X}{:02X}{:02X}", value.red, value.green, value.blue),
    )
}

fn mode_text(mode: Option<LightMode>) -> &'static str {
    match mode {
        None => "unknown",
        Some(LightMode::Temperature) => "temperature",
        Some(LightMode::Rgb) => "RGB",
    }
}

fn temperature_range(capabilities: &[Capability]) -> Option<crate::device::NumericRange> {
    capabilities.iter().find_map(|capability| match capability {
        Capability::ColorTemperature(range) => Some(*range),
        _ => None,
    })
}

fn has_color(capabilities: &[Capability]) -> bool {
    temperature_range(capabilities).is_some() || capabilities.contains(&Capability::ColorRgb)
}

/// Render one function block from a snapshot the caller trusts: the live cache
/// for standalone devices, the fresh applied snapshot for group members.
fn render_function(function: &Function, snapshot: &FunctionSnapshot, lines: &mut Vec<String>) {
    let id = &function.function_key().id;
    let description = function.description();
    let label = &description.label;
    if description.capabilities.contains(&Capability::OnOff) {
        lines.push(format!("  {id} ({label}): {}", power_text(snapshot.power)));
    } else {
        lines.push(format!("  {id} ({label})"));
    }
    if description.category == Category::Light {
        lines.push(format!(
            "    Capabilities: {}.",
            capability_list(&description.capabilities)
        ));
    }
    if let Some(lighting) = snapshot.lighting {
        let mut values = Vec::new();
        if description.capabilities.contains(&Capability::Brightness) {
            values.push(format!(
                "Brightness: {}",
                percent(lighting.brightness_percent)
            ));
        }
        if description.temperature_range().is_some() {
            values.push(format!(
                "temperature: {}",
                kelvin(lighting.temperature_kelvin)
            ));
        }
        if description.capabilities.contains(&Capability::ColorRgb) {
            values.push(format!("RGB: {}", color(lighting.color)));
        }
        if has_color(&description.capabilities) {
            values.push(format!("mode: {}", mode_text(lighting.mode)));
        }
        if !values.is_empty() {
            lines.push(format!("    {}.", values.join("; ")));
        }
    }
}

fn render_device(device: &PhysicalDevice, lines: &mut Vec<String>) {
    let key = device.key();
    lines.push(format!(
        "{} ({}): {}",
        key.id,
        device.label(),
        availability_text(&device.availability())
    ));
    for function in device.functions() {
        render_function(function, &function.snapshot(), lines);
    }
}

fn render_live(device: &PhysicalDevice) -> String {
    let mut lines = Vec::new();
    render_device(device, &mut lines);
    lines.join("\n")
}

/// The aggregate state of one applied group. Capabilities and the shared
/// temperature range come from the applied description; every value comes from
/// [`AppliedGroup::snapshot`], so an invalidated member cache is unknown here.
fn group_state(description: &GroupDescription, snapshot: &GroupSnapshot) -> String {
    let capabilities = &description.capabilities;
    let lighting = snapshot.lighting;
    let mut values = Vec::new();
    if capabilities.contains(&Capability::OnOff) {
        values.push(format!("power {}", power_text(snapshot.power)));
    }
    if capabilities.contains(&Capability::Brightness) {
        values.push(format!(
            "brightness {}",
            percent(lighting.and_then(|lighting| lighting.brightness_percent))
        ));
    }
    if temperature_range(capabilities).is_some() {
        values.push(format!(
            "temperature {}",
            kelvin(lighting.and_then(|lighting| lighting.temperature_kelvin))
        ));
    }
    if capabilities.contains(&Capability::ColorRgb) {
        values.push(format!(
            "RGB: {}",
            color(lighting.and_then(|lighting| lighting.color))
        ));
    }
    if has_color(capabilities) {
        values.push(format!(
            "mode {}",
            mode_text(lighting.and_then(|lighting| lighting.mode))
        ));
    }
    values.push(format!(
        "representative: {}",
        snapshot
            .representative
            .as_ref()
            .map_or("unknown", |key| key.device.id.as_str())
    ));
    values.push(format!("reachable: {}", snapshot.reachable));
    values.join("; ")
}

fn render_group(group: &AppliedGroup, labels: &Labels, lines: &mut Vec<String>) {
    let members = member_names(labels, &group.members);
    lines.push(format!(
        "{} ({}): group; members: {members}",
        group.key.id, group.label
    ));
    let description = group.description();
    lines.push(format!(
        "  Capabilities: {}.",
        capability_list(&description.capabilities)
    ));
    lines.push(format!(
        "  State: {}.",
        group_state(&description, &group.snapshot())
    ));
}

/// Member blocks render only the fresh observations of the applied handle.
/// A member whose source path is gone is explicitly removed or closed instead
/// of borrowing another instance or synthesizing a default state.
fn render_group_members(
    group: &AppliedGroup,
    labels: &Labels,
    snapshot: &RegistrySnapshot,
    only: Option<&str>,
    lines: &mut Vec<String>,
) {
    let group_snapshot = group.snapshot();
    for function in group.functions() {
        let member = &function.function_key().device;
        if only.is_some_and(|only| member.id != only) {
            continue;
        }
        let fresh = group_snapshot
            .members
            .iter()
            .find(|fresh| &fresh.key.device == member)
            .expect("group functions and snapshot share one member set");
        let live = snapshot
            .devices
            .iter()
            .find(|device| device.key() == member);
        let status = if fresh.reachable {
            availability_text(&fresh.snapshot.availability)
        } else {
            match live {
                None => "removed from source".into(),
                Some(device) if device.availability() == Availability::Available => "closed".into(),
                Some(device) => availability_text(&device.availability()),
            }
        };
        let label = labels.get(member).map(String::as_str).unwrap_or(&member.id);
        lines.push(format!("{} ({label}): {status}", member.id));
        let groups = snapshot
            .applied
            .groups
            .iter()
            .filter(|candidate| {
                if only.is_some() {
                    candidate.members.contains(member)
                } else {
                    candidate.key == group.key
                }
            })
            .map(|group| format!("{} ({})", group.key.id, group.label))
            .collect::<Vec<_>>();
        let heading = if groups.len() == 1 { "Group" } else { "Groups" };
        lines.push(format!("  {heading}: {}.", groups.join(", ")));
        render_function(&function, &fresh.snapshot, lines);
    }
}

/// Render the applied publication set from one registry snapshot. Groups use
/// their stable aggregate handle; standalone devices keep their applied source
/// instance until the next commit. A selected member keeps only its own block.
fn render_applied(snapshot: &RegistrySnapshot, labels: &Labels, did: Option<&str>) -> String {
    let mut lines = Vec::new();
    for group in &snapshot.applied.groups {
        let group_selected = did.is_none_or(|did| group.key.id == did);
        let member_selected =
            did.is_some_and(|did| group.members.iter().any(|member| member.id == did));
        if !group_selected && !member_selected {
            continue;
        }
        if group_selected {
            render_group(group, labels, &mut lines);
        }
        let only = if group_selected { None } else { did };
        render_group_members(group, labels, snapshot, only, &mut lines);
        if member_selected && !group_selected {
            break;
        }
    }
    for applied in &snapshot.applied.devices {
        let key = applied.key();
        if did.is_some_and(|did| did != key.id) {
            continue;
        }
        match snapshot.devices.iter().find(|device| device.key() == key) {
            Some(device) if device.same_instance(applied) => render_device(applied, &mut lines),
            Some(_) => {
                lines.push(format!("{} ({}): closed", key.id, applied.label()));
                for function in applied.functions() {
                    render_function(function, &function.fresh_snapshot().snapshot, &mut lines);
                }
            }
            None => {
                let label = labels.get(key).map(String::as_str).unwrap_or(&key.id);
                lines.push(format!("{} ({label}): removed from source.", key.id));
            }
        }
    }
    if lines.is_empty() {
        "No devices are currently connected to the bridge.".into()
    } else {
        lines.join("\n")
    }
}

/// A confirmed candidate group without an applied endpoint is never called
/// published and never executes commands.
fn render_candidate(group: &GroupConfiguration, labels: &Labels) -> String {
    let members = member_names(labels, &group.members);
    format!(
        "{} ({}): waiting to apply; not published.\n  Capabilities: {}.\n  Members: {members}.",
        group.key.id,
        group.label,
        capability_list(&group.capabilities)
    )
}

fn not_published(snapshot: &RegistrySnapshot, did: &str) -> String {
    if let Some(pending) = &snapshot.pending {
        let groups = pending
            .groups
            .iter()
            .filter(|group| group.members.iter().any(|member| member.id == did))
            .map(|group| group.key.id.as_str())
            .collect::<Vec<_>>();
        if !groups.is_empty() {
            return format!(
                "  Not published: candidate member of {} (waiting to apply).",
                groups.join(", ")
            );
        }
        if pending.devices.iter().any(|device| device.key().id == did) {
            return "  Not published: waiting to apply.".into();
        }
    }
    "  Not published: not part of the applied configuration.".into()
}

fn format_pending(pending: &PendingDevice, name: &str) -> String {
    if pending.group {
        format!(
            "{} ({}): pending group ({})",
            pending.did, name, pending.reason
        )
    } else {
        format!("{} ({}): pending ({})", pending.did, name, pending.reason)
    }
}
/// The reserved saved name outlives a revoked or renamed cloud relation; the
/// storage error propagates to the terminal caller instead of being hidden.
fn pending_name(store: &DeviceStore, pending: &PendingDevice) -> Result<String, StorageError> {
    if pending.group
        && let Some(label) = store.load_group_label(&DeviceKey::new("xiaomi", &pending.did))?
    {
        return Ok(label);
    }
    Ok(pending.name.clone())
}
/// Terminal on/off commands select physical member DIDs.
fn group_command_refusal(status: &AuthStatus, now: i64, did: &str, member: Option<&str>) -> String {
    let example = member.map_or_else(String::new, |member| format!(", for example {member}"));
    invalid(
        status,
        now,
        &format!("{did} is a light group; select a member DID for on/off{example}."),
    )
}
pub async fn handle_line(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Result<Option<String>, StorageError> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    let snapshot = registry.snapshot();
    let text = match words.as_slice() {
        [] => return Ok(None),
        ["help"] => format!("{COMMAND_HELP}\n{}", status.render(now)),
        ["list"] => {
            let labels = labels(store, &snapshot)?;
            let mut text = render_applied(&snapshot, &labels, None);
            for pending in status.pending() {
                let name = if pending.group {
                    pending_name(store, &pending)?
                } else {
                    let key = DeviceKey::new("xiaomi", &pending.did);
                    match snapshot.devices.iter().find(|device| device.key() == &key) {
                        Some(physical) => physical.label(),
                        None => store
                            .load_published(&key)?
                            .map(|stored| stored.label)
                            .unwrap_or_else(|| pending.name.clone()),
                    }
                };
                text.push_str(&format!("\n{}", format_pending(&pending, &name)));
            }
            text
        }
        ["status"] => {
            let labels = labels(store, &snapshot)?;
            let mut text = render_applied(&snapshot, &labels, None);
            text.push('\n');
            text.push_str(&status.render_devices(now, None, &snapshot));
            text
        }
        ["status", did] => {
            let labels = labels(store, &snapshot)?;
            let applied_group = snapshot
                .applied
                .groups
                .iter()
                .find(|group| group.key.id == *did);
            let applied_member = snapshot
                .applied
                .groups
                .iter()
                .any(|group| group.members.iter().any(|member| member.id == *did));
            let applied_standalone = snapshot
                .applied
                .devices
                .iter()
                .any(|device| device.key().id == *did);
            let candidate = snapshot
                .pending
                .as_ref()
                .and_then(|pending| pending.groups.iter().find(|group| group.key.id == *did));
            let live = snapshot
                .devices
                .iter()
                .find(|device| device.key().id == *did);
            let adapter_pending = status
                .pending()
                .into_iter()
                .find(|pending| pending.did == *did);
            let applied = applied_group.is_some() || applied_member || applied_standalone;
            let mut sections = if applied {
                vec![render_applied(&snapshot, &labels, Some(did))]
            } else if let Some(device) = live {
                vec![render_live(device), not_published(&snapshot, did)]
            } else if let Some(group) = candidate {
                vec![render_candidate(group, &labels)]
            } else if adapter_pending.is_some() {
                Vec::new()
            } else {
                return Ok(Some(invalid(status, now, "Unknown device DID.")));
            };
            if let Some(pending) = &adapter_pending {
                sections.push(format_pending(pending, &pending_name(store, pending)?));
            }
            sections.push(if let Some(group) = applied_group {
                let members = group
                    .members
                    .iter()
                    .map(|member| member.id.clone())
                    .collect::<Vec<_>>();
                status.render_group_devices(now, &members, &snapshot)
            } else if applied || live.is_some() {
                status.render_devices(now, Some(did), &snapshot)
            } else {
                status.render(now)
            });
            sections.join("\n")
        }
        [verb @ ("on" | "off"), did, channel @ ..] if channel.len() <= 1 => {
            let applied_group = snapshot
                .applied
                .groups
                .iter()
                .find(|group| group.key.id == *did);
            let candidate_group = snapshot
                .pending
                .as_ref()
                .and_then(|pending| pending.groups.iter().find(|group| group.key.id == *did));
            let pending_group = status
                .pending()
                .into_iter()
                .find(|pending| pending.did == *did && pending.group);
            if let Some(group) = applied_group {
                return Ok(Some(group_command_refusal(
                    status,
                    now,
                    did,
                    group.members.first().map(|member| member.id.as_str()),
                )));
            }
            if candidate_group.is_some() || pending_group.is_some() {
                return Ok(Some(invalid(
                    status,
                    now,
                    &format!("{did} is a light group waiting to apply; it is not published yet."),
                )));
            }
            let Some(physical) = registry.get(&DeviceKey::new("xiaomi", *did)) else {
                return Ok(Some(invalid(status, now, "Unknown device DID.")));
            };
            let function = if let Some(channel) = channel.first() {
                physical.function(channel)
            } else if let [function] = physical.functions() {
                Some(function.clone())
            } else {
                return Ok(Some(invalid(
                    status,
                    now,
                    "A channel is required for this device.",
                )));
            };
            let Some(function) = function else {
                return Ok(Some(invalid(status, now, "Unknown channel.")));
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
        _ => invalid(status, now, "Invalid command usage."),
    };
    Ok(Some(text))
}
/// EOF ends input while the caller continues polling every bridge service.
pub async fn run_input(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    mut input: impl futures_lite::io::AsyncBufRead + Unpin,
    mut output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if let Some(result) = handle_line(registry, store, status, &line, current_time()).await? {
            output.write_all(result.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
}

#[cfg(test)]
mod tests;
