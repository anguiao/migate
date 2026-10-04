use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{Availability, Command, DeviceKey, DeviceRegistry, Power},
    storage::{DeviceStore, StorageError},
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID [channel], off DID [channel], help\nUse list to find stable channel keys. A single-channel device may omit channel; channel is required for a multi-channel device.";
fn invalid(status: &AuthStatus, now: i64, reason: &str) -> String {
    format!("{reason}\n{COMMAND_HELP}\n{}", status.render(now))
}
fn render(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    did: Option<&str>,
) -> Result<String, StorageError> {
    let mut lines = Vec::new();
    for physical in registry
        .list()
        .into_iter()
        .filter(|physical| did.is_none_or(|did| physical.description().key.id == did))
    {
        let key = &physical.description().key;
        let stored = store.load_published(key)?.ok_or_else(|| {
            store.invalid(
                "read bridge device",
                "Registered device has no published identity",
            )
        })?;
        let availability = match physical.availability() {
            Availability::Initializing => "initializing".into(),
            Availability::Available => "available".into(),
            Availability::Unavailable(reason) => format!("unavailable ({reason})"),
        };
        lines.push(format!("{} ({}): {availability}", key.id, stored.label));
        for function in physical.functions() {
            let id = &function.function_key().id;
            let label = &stored
                .functions
                .iter()
                .find(|stored| &stored.key.id == id)
                .ok_or_else(|| {
                    store.invalid(
                        "read bridge function",
                        "Registered function has no stored identity",
                    )
                })?
                .label;
            let power = match function.snapshot().power {
                Power::Unknown => "unknown",
                Power::On => "on",
                Power::Off => "off",
            };
            lines.push(format!("  {id} ({label}): {power}"));
        }
    }
    Ok(if lines.is_empty() {
        "No devices are currently connected to the bridge.".into()
    } else {
        lines.join("\n")
    })
}
pub async fn handle_line(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    line: &str,
    now: i64,
) -> Result<Option<String>, StorageError> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    let Some(verb) = words.first().copied() else {
        return Ok(None);
    };
    let text = match (verb, words.as_slice()) {
        ("help", [_]) => format!("{COMMAND_HELP}\n{}", status.render(now)),
        ("list", [_]) => {
            let mut text = render(registry, store, None)?;
            for pending in status.pending() {
                let saved = store.load_published(&DeviceKey::new("xiaomi", &pending.did))?;
                let name = saved
                    .as_ref()
                    .map(|stored| stored.label.as_str())
                    .unwrap_or(&pending.name);
                text.push_str(&format!(
                    "\n{} ({}): pending ({})",
                    pending.did, name, pending.reason
                ));
            }
            text
        }
        ("status", [_]) => format!("{}\n{}", render(registry, store, None)?, status.render(now)),
        ("status", [_, did]) => {
            if registry.get(&DeviceKey::new("xiaomi", *did)).is_none() {
                invalid(status, now, "Unknown device DID.")
            } else {
                format!(
                    "{}\n{}",
                    render(registry, store, Some(did))?,
                    status.render(now)
                )
            }
        }
        ("on" | "off", [_, did] | [_, did, _]) => {
            let Some(physical) = registry.get(&DeviceKey::new("xiaomi", *did)) else {
                return Ok(Some(invalid(status, now, "Unknown device DID.")));
            };
            let function = if words.len() == 3 {
                physical.function(words[2])
            } else if physical.functions().len() == 1 {
                Some(physical.functions()[0].clone())
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
                .execute(if verb == "on" {
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
