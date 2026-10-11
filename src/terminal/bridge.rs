use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{
        Availability, Capability, Category, Command, Composition, DeviceKey, DeviceRegistry,
        LightMode, PhysicalDevice, Power, RegistrySnapshot,
    },
    storage::{DeviceStore, StorageError},
    xiaomi::adapter::PendingDevice,
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID [channel], off DID [channel], help\nUse list to find stable channel keys. A single-channel device may omit channel; channel is required for a multi-channel device.";
fn invalid(status: &AuthStatus, now: i64, reason: &str) -> String {
    format!("{reason}\n{COMMAND_HELP}\n{}", status.render(now))
}
fn member_names(snapshot: &RegistrySnapshot, group: &Composition) -> String {
    group
        .members
        .iter()
        .map(|member| {
            let device = snapshot
                .devices
                .iter()
                .find(|device| device.key() == member)
                .expect("composition member is published");
            format!("{} ({})", member.id, device.label())
        })
        .collect::<Vec<_>>()
        .join(", ")
}
fn group_of<'a>(snapshot: &'a RegistrySnapshot, key: &DeviceKey) -> Option<&'a Composition> {
    snapshot
        .composition
        .groups
        .iter()
        .find(|group| group.members.iter().any(|member| member == key))
}
fn render_device(device: &PhysicalDevice, group: Option<&Composition>, lines: &mut Vec<String>) {
    let key = device.key();
    let availability = match device.availability() {
        Availability::Initializing => "initializing".into(),
        Availability::Available => "available".into(),
        Availability::Unavailable(reason) => format!("unavailable ({reason})"),
    };
    lines.push(format!("{} ({}): {availability}", key.id, device.label()));
    if let Some(group) = group {
        lines.push(format!("  Group: {} ({}).", group.key.id, group.label));
    }
    for function in device.functions() {
        let id = &function.function_key().id;
        let description = function.description();
        let label = &description.label;
        let snapshot = function.snapshot();
        let capabilities = description
            .capabilities
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
            .join(", ");
        if description.capabilities.contains(&Capability::OnOff) {
            let power = match snapshot.power {
                Power::Unknown => "unknown",
                Power::On => "on",
                Power::Off => "off",
            };
            lines.push(format!("  {id} ({label}): {power}"));
        } else {
            lines.push(format!("  {id} ({label})"));
        }
        if description.category == Category::Light {
            lines.push(format!("    Capabilities: {capabilities}."));
        }
        if let Some(light) = snapshot.lighting {
            let mut values = Vec::new();
            if description.capabilities.contains(&Capability::Brightness) {
                values.push(format!(
                    "Brightness: {}",
                    light
                        .brightness_percent
                        .map_or_else(|| "unknown".into(), |value| format!("{value}%"))
                ));
            }
            if description.temperature_range().is_some() {
                values.push(format!(
                    "temperature: {}",
                    light
                        .temperature_kelvin
                        .map_or_else(|| "unknown".into(), |value| format!("{value} K"))
                ));
            }
            if description.capabilities.contains(&Capability::ColorRgb) {
                values.push(format!(
                    "RGB: {}",
                    light.color.map_or_else(
                        || "unknown".into(),
                        |value| format!("#{:02X}{:02X}{:02X}", value.red, value.green, value.blue)
                    )
                ));
            }
            if description.temperature_range().is_some()
                || description.capabilities.contains(&Capability::ColorRgb)
            {
                values.push(format!(
                    "mode: {}",
                    match light.mode {
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
    }
}
/// Render devices and group relations from one registry snapshot. A selected
/// group renders its member blocks; a selected member keeps its group line.
fn render(snapshot: &RegistrySnapshot, did: Option<&str>) -> String {
    let mut lines = Vec::new();
    for group in snapshot
        .composition
        .groups
        .iter()
        .filter(|group| did.is_none_or(|did| group.key.id == did))
    {
        lines.push(format!(
            "{} ({}): group; members: {}",
            group.key.id,
            group.label,
            member_names(snapshot, group)
        ));
        if did.is_some() {
            for member in &group.members {
                let device = snapshot
                    .devices
                    .iter()
                    .find(|device| device.key() == member)
                    .expect("composition member is published");
                render_device(device, None, &mut lines);
            }
        }
    }
    for device in snapshot
        .devices
        .iter()
        .filter(|device| did.is_none_or(|did| device.key().id == did))
    {
        render_device(device, group_of(snapshot, device.key()), &mut lines);
    }
    if lines.is_empty() {
        "No devices are currently connected to the bridge.".into()
    } else {
        lines.join("\n")
    }
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
/// A group DID names no controllable endpoint; on/off must select a member.
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
            let mut text = render(&snapshot, None);
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
        ["status"] => format!(
            "{}\n{}",
            render(&snapshot, None),
            status.render_devices(now, None, &snapshot)
        ),
        ["status", did] => {
            let group = snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key.id == *did);
            let physical = snapshot
                .devices
                .iter()
                .find(|device| device.key().id == *did);
            let pending = status
                .pending()
                .into_iter()
                .find(|pending| pending.did == *did);
            if let Some(group) = group {
                let members = group
                    .members
                    .iter()
                    .map(|member| member.id.clone())
                    .collect::<Vec<_>>();
                let mut text = render(&snapshot, Some(did));
                if let Some(pending) = &pending {
                    text.push_str(&format!(
                        "\n{}",
                        format_pending(pending, &pending_name(store, pending)?)
                    ));
                }
                text.push('\n');
                text.push_str(&status.render_group_devices(now, &members, &snapshot));
                text
            } else if physical.is_some() {
                let mut text = render(&snapshot, Some(did));
                if let Some(pending) = &pending {
                    text.push_str(&format!(
                        "\n{}",
                        format_pending(pending, &pending_name(store, pending)?)
                    ));
                }
                text.push('\n');
                text.push_str(&status.render_devices(now, Some(did), &snapshot));
                text
            } else if let Some(pending) = &pending {
                format!(
                    "{}\n{}",
                    format_pending(pending, &pending_name(store, pending)?),
                    status.render(now)
                )
            } else {
                invalid(status, now, "Unknown device DID.")
            }
        }
        [verb @ ("on" | "off"), did, channel @ ..] if channel.len() <= 1 => {
            let group = snapshot
                .composition
                .groups
                .iter()
                .find(|group| group.key.id == *did);
            let pending_group = status
                .pending()
                .into_iter()
                .find(|pending| pending.did == *did && pending.group);
            if group.is_some() || pending_group.is_some() {
                let member = group
                    .and_then(|group| group.members.first())
                    .map(|member| member.id.as_str());
                return Ok(Some(group_command_refusal(status, now, did, member)));
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
