mod context;
mod render;
pub(crate) use context::Context;
use context::View;
use render::{configuration_lines, render};

use super::AuthStatus;
use crate::{
    RuntimeError,
    device::{Command, DeviceKey, FunctionKey, FunctionKind, HeaterCommand, HeaterMode},
    storage::StorageError,
    xiaomi::adapter::AdapterStatus,
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID FUNCTION, off DID FUNCTION, map-type DID FUNCTION switch|light, mode DID FUNCTION off|heat, temperature DID FUNCTION VALUE, bind-sensor TARGET_DID TARGET_FUNCTION SENSOR_DID SENSOR_FUNCTION, unbind-sensor TARGET_DID TARGET_FUNCTION, help\nUse list to find stable function keys.";
impl View {
    async fn handle_words(
        &self,
        words: &[&str],
        status: &AuthStatus,
        diagnostics: impl FnOnce() -> AdapterStatus,
    ) -> Result<Option<String>, StorageError> {
        let diagnostics = std::cell::LazyCell::new(diagnostics);
        let invalid = |reason: &str| {
            format!(
                "{reason}\n{COMMAND_HELP}\n{}",
                self.render_status(status, &diagnostics)
            )
        };
        let text = match words {
            [] => return Ok(None),
            ["help"] => format!(
                "{COMMAND_HELP}\n{}",
                self.render_status(status, &diagnostics)
            ),
            ["list"] => {
                let mut text = render(self, None, &diagnostics, true);
                for pending in &diagnostics.pending {
                    text.push('\n');
                    text.push_str(&self.pending_line(pending));
                }
                text
            }
            ["status"] => format!(
                "{}\n{}",
                render(self, None, &diagnostics, false),
                self.render_devices(status, &diagnostics, None)
            ),
            ["status", did] => {
                if self.get(&DeviceKey::new("xiaomi", *did)).is_none() {
                    if self.contains_did(did) {
                        let mut lines = vec![format!("{did}: not published")];
                        lines.extend(configuration_lines(self, did));
                        lines.push(self.render_devices(status, &diagnostics, Some(did)));
                        lines.join("\n")
                    } else if let Some(pending) = diagnostics
                        .pending
                        .iter()
                        .find(|pending| pending.did == *did)
                    {
                        self.pending_line(pending)
                    } else {
                        invalid("Unknown device DID.")
                    }
                } else {
                    format!(
                        "{}\n{}",
                        render(self, Some(did), &diagnostics, false),
                        self.render_devices(status, &diagnostics, Some(did))
                    )
                }
            }
            [verb @ ("on" | "off"), did, id] => {
                let Some(physical) = self.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Ok(Some(invalid("Unknown device DID.")));
                };
                let Some(function) = physical.function(id) else {
                    return Ok(Some(invalid("Unknown function.")));
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
            [verb @ "mode", did, id, value @ ("off" | "heat")]
            | [verb @ "temperature", did, id, value] => {
                let Some(physical) = self.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Ok(Some(invalid("Unknown device DID.")));
                };
                let Some(function) = physical.function(id) else {
                    return Ok(Some(invalid("Unknown function.")));
                };
                if !matches!(function.description().kind, FunctionKind::Heater { .. }) {
                    return Ok(Some(invalid(&format!(
                        "Function does not support heating {verb} control."
                    ))));
                }
                let (command, value) = if *verb == "mode" {
                    let mode = if *value == "off" {
                        HeaterMode::Off
                    } else {
                        HeaterMode::Heat
                    };
                    (HeaterCommand::Mode(mode), value.to_string())
                } else {
                    let Ok(value) = value.parse::<i16>() else {
                        return Ok(Some(invalid("Temperature must be a whole number.")));
                    };
                    (HeaterCommand::Temperature(value), value.to_string())
                };
                let target = if physical.functions().len() == 1 {
                    did.to_string()
                } else {
                    format!("{did}/{id}")
                };
                let result = match function.begin_heater(command) {
                    Ok(action) => action.accepted().await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(_) => {
                        log::info!("terminal {target} {verb} {value}: accepted");
                        format!("{target}: command accepted")
                    }
                    Err(error) => format!("{target}: {error}"),
                }
            }
            _ => invalid("Invalid command usage."),
        };
        Ok(Some(text))
    }
}

impl Context<'_> {
    pub(crate) async fn handle_line(&self, line: &str) -> Result<Option<String>, StorageError> {
        let words = line.split_whitespace().collect::<Vec<_>>();
        match words.as_slice() {
            ["map-type", did, id, kind] => {
                let device_type = match *kind {
                    "switch" => crate::configuration::RelayType::Switch,
                    "light" => crate::configuration::RelayType::Light,
                    _ => return Ok(Some("Unknown mapping type; use switch or light.".into())),
                };
                let key = FunctionKey::new(DeviceKey::new("xiaomi", *did), *id);
                Ok(Some(match self.app.map_relay_type(&key, device_type) {
                    Ok(true) => format!("{did}/{id}: mapped to {kind}."),
                    Ok(false) => format!("{did}/{id}: already mapped to {kind}."),
                    Err(error) => format!("Mapping failed: {error}"),
                }))
            }
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
            _ => {
                self.view()
                    .handle_words(&words, self.status, || self.app.diagnostics())
                    .await
            }
        }
    }
}

impl Context<'_> {
    pub(crate) async fn run_input(
        &self,
        input: impl futures_lite::io::AsyncBufRead + Unpin,
        output: impl futures_lite::io::AsyncWrite + Unpin,
    ) -> Result<(), RuntimeError> {
        read_commands(input, output, async |line| self.handle_line(line).await).await
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
