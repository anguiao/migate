use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{Availability, Capability, Command, DeviceKey, DeviceRegistry, LightMode, Power},
    storage::{DeviceStore, StorageError},
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID [channel], off DID [channel], help\nUse list to find stable channel keys. A single-channel device may omit channel; channel is required for a multi-channel device.";
fn invalid(status: &AuthStatus, now: i64, reason: &str) -> String {
    format!("{reason}\n{COMMAND_HELP}\n{}", status.render(now))
}
fn render(registry: &DeviceRegistry, did: Option<&str>) -> String {
    let mut lines = Vec::new();
    for physical in registry
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
        for function in physical.functions() {
            let id = &function.function_key().id;
            let label = &function.description().label;
            let snapshot = function.snapshot();
            let power = match snapshot.power {
                Power::Unknown => "unknown",
                Power::On => "on",
                Power::Off => "off",
            };
            lines.push(format!("  {id} ({label}): {power}"));
            if let Some(light) = snapshot.lighting {
                let description = function.description();
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
                lines.push(format!("    Capabilities: {capabilities}."));
                let mut values = Vec::new();
                if description.has_capability(Capability::Brightness) {
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
                if description.has_capability(Capability::ColorRgb) {
                    values.push(format!(
                        "RGB: {}",
                        light.color.map_or_else(
                            || "unknown".into(),
                            |value| format!(
                                "#{:02X}{:02X}{:02X}",
                                value.red, value.green, value.blue
                            )
                        )
                    ));
                }
                if description.temperature_range().is_some()
                    || description.has_capability(Capability::ColorRgb)
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
    if lines.is_empty() {
        "No devices are currently connected to the bridge.".into()
    } else {
        lines.join("\n")
    }
}
pub async fn handle_line(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Result<Option<String>, StorageError> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    let text = match words.as_slice() {
        [] => return Ok(None),
        ["help"] => format!("{COMMAND_HELP}\n{}", status.render(now)),
        ["list"] => {
            let mut text = render(registry, None);
            for pending in status.pending() {
                let key = DeviceKey::new("xiaomi", &pending.did);
                let name = match registry.get(&key) {
                    Some(physical) => physical.label(),
                    None => store
                        .load_published(&key)?
                        .map(|stored| stored.label)
                        .unwrap_or_else(|| pending.name.clone()),
                };
                text.push_str(&format!(
                    "\n{} ({}): pending ({})",
                    pending.did, name, pending.reason
                ));
            }
            text
        }
        ["status"] => format!(
            "{}\n{}",
            render(registry, None),
            status.render_devices(now, None)
        ),
        ["status", did] => {
            if registry.get(&DeviceKey::new("xiaomi", *did)).is_none() {
                invalid(status, now, "Unknown device DID.")
            } else {
                format!(
                    "{}\n{}",
                    render(registry, Some(did)),
                    status.render_devices(now, Some(did))
                )
            }
        }
        [verb @ ("on" | "off"), did, channel @ ..] if channel.len() <= 1 => {
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
