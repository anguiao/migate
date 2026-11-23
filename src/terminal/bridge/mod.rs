mod context;
mod management;
mod render;
pub(crate) use context::Context;
use context::View;
pub(crate) use management::Management;
use render::render;

use super::AuthStatus;
use crate::{
    device::{Command, DeviceKey, FunctionKey, FunctionKind, HeaterCommand, HeaterMode},
    storage::StorageError,
    xiaomi::adapter::AdapterStatus,
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID FUNCTION, off DID FUNCTION, map-type DID FUNCTION switch|light, mode DID FUNCTION off|heat, temperature DID FUNCTION VALUE, bind-sensor TARGET_DID TARGET_FUNCTION SENSOR_DID SENSOR_FUNCTION, unbind-sensor TARGET_DID TARGET_FUNCTION, help\nUse list to find stable function keys.";

pub(crate) fn is_command(verb: &str) -> bool {
    matches!(
        verb,
        "list"
            | "status"
            | "on"
            | "off"
            | "map-type"
            | "mode"
            | "temperature"
            | "bind-sensor"
            | "unbind-sensor"
            | "help"
    )
}

struct Reply {
    success: bool,
    text: String,
}

impl Reply {
    fn success(text: impl Into<String>) -> Self {
        Self {
            success: true,
            text: text.into(),
        }
    }

    fn failure(text: impl Into<String>) -> Self {
        Self {
            success: false,
            text: text.into(),
        }
    }
}

impl View {
    async fn handle_words(
        &self,
        words: &[&str],
        status: &AuthStatus,
        diagnostics: impl FnOnce() -> AdapterStatus,
    ) -> Option<Reply> {
        let diagnostics = std::cell::LazyCell::new(diagnostics);
        let invalid = |reason: &str| {
            Reply::failure(format!(
                "{reason}\n{COMMAND_HELP}\n{}",
                self.render_status(status, &diagnostics)
            ))
        };
        let missing = |did| {
            if let Some(pending) = diagnostics.pending.iter().find(|record| record.did == did) {
                Reply::failure(self.pending_line(pending))
            } else {
                invalid("Unknown device DID.")
            }
        };
        let reply = match words {
            [] => return None,
            ["help"] => Reply::success(format!(
                "{COMMAND_HELP}\n{}",
                self.render_status(status, &diagnostics)
            )),
            ["list"] => {
                let mut text = render(self, None, &diagnostics, true);
                for pending in &diagnostics.pending {
                    text.push('\n');
                    text.push_str(&self.pending_line(pending));
                }
                Reply::success(text)
            }
            ["status"] => Reply::success(format!(
                "{}\n{}",
                render(self, None, &diagnostics, false),
                self.render_devices(status, &diagnostics, None)
            )),
            ["status", did] => render::device_status(self, did, status, &diagnostics)
                .map(Reply::success)
                .unwrap_or_else(|| invalid("Unknown device DID.")),
            [verb @ ("on" | "off"), did, id] => {
                let Some(physical) = self.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Some(missing(*did));
                };
                let Some(function) = physical.function(id) else {
                    return Some(invalid("Unknown function."));
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
                        Reply::success(format!("{target}: command accepted"))
                    }
                    Err(error) => Reply::failure(format!("{target}: {error}")),
                }
            }
            [verb @ "mode", did, id, value @ ("off" | "heat")]
            | [verb @ "temperature", did, id, value] => {
                let Some(physical) = self.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Some(missing(*did));
                };
                let Some(function) = physical.function(id) else {
                    return Some(invalid("Unknown function."));
                };
                if !matches!(function.description().kind, FunctionKind::Heater { .. }) {
                    return Some(invalid(&format!(
                        "Function does not support heating {verb} control."
                    )));
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
                        return Some(invalid("Temperature must be a whole number."));
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
                        Reply::success(format!("{target}: command accepted"))
                    }
                    Err(error) => Reply::failure(format!("{target}: {error}")),
                }
            }
            _ => invalid("Invalid command usage."),
        };
        Some(reply)
    }
}

impl Context<'_> {
    async fn handle_words(&self, words: &[&str]) -> Result<Option<Reply>, StorageError> {
        match words {
            ["map-type", did, id, kind] => {
                let device_type = match *kind {
                    "switch" => crate::configuration::RelayType::Switch,
                    "light" => crate::configuration::RelayType::Light,
                    _ => {
                        return Ok(Some(Reply::failure(
                            "Unknown mapping type; use switch or light.",
                        )));
                    }
                };
                let key = FunctionKey::new(DeviceKey::new("xiaomi", *did), *id);
                Ok(Some(match self.app.map_relay_type(&key, device_type) {
                    Ok(true) => Reply::success(format!("{did}/{id}: mapped to {kind}.")),
                    Ok(false) => Reply::success(format!("{did}/{id}: already mapped to {kind}.")),
                    Err(error) => Reply::failure(format!("Mapping failed: {error}")),
                }))
            }
            ["bind-sensor", target_did, target_id, sensor_did, sensor_id] => {
                let target = FunctionKey::new(DeviceKey::new("xiaomi", *target_did), *target_id);
                let source = FunctionKey::new(DeviceKey::new("xiaomi", *sensor_did), *sensor_id);
                let result = self.app.bind_sensor(target, source);
                Ok(Some(match result {
                    Ok(true) => Reply::success(format!(
                        "{target_did}/{target_id}: bound to {sensor_did}/{sensor_id}."
                    )),
                    Ok(false) => Reply::success(format!(
                        "{target_did}/{target_id}: already bound to {sensor_did}/{sensor_id}."
                    )),
                    Err(error) => Reply::failure(format!("Sensor binding failed: {error}")),
                }))
            }
            ["unbind-sensor", target_did, target_id] => {
                let target = FunctionKey::new(DeviceKey::new("xiaomi", *target_did), *target_id);
                let result = self.app.unbind_sensor(&target);
                Ok(Some(match result {
                    Ok(true) => {
                        Reply::success(format!("{target_did}/{target_id}: sensor unbound."))
                    }
                    Ok(false) => {
                        Reply::success(format!("{target_did}/{target_id}: sensor already unbound."))
                    }
                    Err(error) => Reply::failure(format!("Sensor unbinding failed: {error}")),
                }))
            }
            _ => Ok(self
                .view()?
                .handle_words(words, self.status, || self.app.diagnostics())
                .await),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
