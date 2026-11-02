mod context;
mod render;
pub(crate) use context::Context;
use context::View;
#[cfg(test)]
use render::measurement_text;
use render::{configuration_lines, render};

use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{Command, DeviceKey, FunctionKey},
    storage::StorageError,
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID FUNCTION, off DID FUNCTION, bind-sensor TARGET_DID TARGET_FUNCTION SENSOR_DID SENSOR_FUNCTION, unbind-sensor TARGET_DID TARGET_FUNCTION, help\nUse list to find stable function keys.";
fn invalid(view: &View, status: &AuthStatus, now: i64, reason: &str) -> String {
    format!(
        "{reason}\n{COMMAND_HELP}\n{}",
        view.render_status(status, now)
    )
}
impl View {
    async fn handle_words(
        &self,
        words: &[&str],
        now: i64,
        status: &AuthStatus,
    ) -> Result<Option<String>, StorageError> {
        let view = self;
        let registry = self;
        let text = match words {
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
        let words = line.split_whitespace().collect::<Vec<_>>();
        match words.as_slice() {
            ["bind-sensor", target_did, target_id, sensor_did, sensor_id] => {
                let target = FunctionKey::new(DeviceKey::new("xiaomi", *target_did), *target_id);
                let source = FunctionKey::new(DeviceKey::new("xiaomi", *sensor_did), *sensor_id);
                let result = self.app.bind_sensor(target, source);
                Ok(Some(match result {
                    Ok(true) => {
                        format!("{target_did}/{target_id}: bound to {sensor_did}/{sensor_id}.")
                    }
                    Ok(false) => format!(
                        "{target_did}/{target_id}: already bound to {sensor_did}/{sensor_id}."
                    ),
                    Err(error) => format!("Sensor binding failed: {error}"),
                }))
            }
            ["unbind-sensor", target_did, target_id] => {
                let target = FunctionKey::new(DeviceKey::new("xiaomi", *target_did), *target_id);
                let result = self.app.unbind_sensor(&target);
                Ok(Some(match result {
                    Ok(true) => format!("{target_did}/{target_id}: sensor unbound."),
                    Ok(false) => format!("{target_did}/{target_id}: sensor already unbound."),
                    Err(error) => format!("Sensor unbinding failed: {error}"),
                }))
            }
            _ => self.view().handle_words(&words, now, self.status).await,
        }
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
