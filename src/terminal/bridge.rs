mod context;
pub(crate) use context::Context;
use context::View;

use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{Availability, Capability, Command, DeviceKey, DeviceRegistry, LightMode, Power},
    storage::{DeviceStore, StorageError, StoredRelation},
};
pub const COMMAND_HELP: &str = "Available commands: list, status [DID], on DID [channel], off DID [channel], help\nUse list to find stable channel keys. A single-channel device may omit channel; channel is required for a multi-channel device.";
fn invalid(view: &View<'_>, status: &AuthStatus, now: i64, reason: &str) -> String {
    format!(
        "{reason}\n{COMMAND_HELP}\n{}",
        view.render_status(status, now)
    )
}
fn configuration_lines(view: &View<'_>, did: &str) -> Vec<String> {
    let Some(view) = view.configuration() else {
        return Vec::new();
    };
    let key = DeviceKey::new("xiaomi", did);
    let mut lines = Vec::new();
    if let Some(record) = view.records.iter().find(|record| record.key == key) {
        lines.push(format!(
            "  Matter: {}; endpoint {}; configuration version {}.",
            if record.published {
                "published"
            } else {
                "hidden or reserved"
            },
            record.endpoint,
            record.configuration_version
        ));
    }
    // The terminal lists every confirmed membership from the effective
    // relations, including groups that are currently unpublished.
    for relation in view
        .source
        .relations
        .iter()
        .filter(|relation| relation.members.iter().any(|member| member == did))
    {
        let group_key = DeviceKey::new("xiaomi", &relation.group_did);
        let state = if view.published.get(&group_key).is_some() {
            "published".to_string()
        } else {
            format!("not published ({})", unpublished_reason(view, relation))
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
    if let Some(group) = view
        .source
        .relations
        .iter()
        .find(|group| group.group_did == did)
    {
        lines.push(format!(
            "  Native group ({}): members {}.",
            if group.is_physical {
                "physical"
            } else {
                "virtual"
            },
            group.members.join(", ")
        ));
        if let Some(entry) = view
            .source
            .entries
            .iter()
            .find(|entry| entry.physical.key() == &key)
        {
            let native = entry.cached.device.functions()[0].description();
            let logical = entry.physical.functions()[0].description();
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
            if entry.repair.is_none()
                && let Some(reason) = &entry.repair_reason
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
            lines.push(format!(
                "  Not published: {}.",
                unpublished_reason(view, group)
            ));
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
            Capability::Brightness => "brightness".into(),
            Capability::ColorTemperature(interval) => {
                format!("temperature {}–{} K", interval.min(), interval.max())
            }
            Capability::ColorRgb => "RGB".into(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn unpublished_reason(
    view: &crate::publication::EffectiveConfiguration,
    relation: &StoredRelation,
) -> String {
    if let Some(error) = &relation.error {
        return error.clone();
    }
    if !relation.active {
        return "confirmed empty relation".into();
    }
    if relation.instance_type.is_none() {
        return "native group description has no instance type".into();
    }
    match view
        .source
        .entries
        .iter()
        .find(|entry| entry.physical.key().id == relation.group_did)
    {
        Some(entry) => entry
            .problem
            .clone()
            .unwrap_or_else(|| "waiting for a usable native description".into()),
        None => "waiting for a usable native description".into(),
    }
}

fn render(registry: &View<'_>, did: Option<&str>) -> String {
    let mut lines = Vec::new();
    if let Some(view) = registry.configuration() {
        for relation in &view.source.relations {
            if did.is_some_and(|did| did != relation.group_did) {
                continue;
            }
            let key = DeviceKey::new("xiaomi", &relation.group_did);
            if registry.get(&key).is_some() {
                continue;
            }
            lines.push(format!(
                "{} (native group): not published ({}).",
                relation.group_did,
                unpublished_reason(view, relation)
            ));
        }
    }
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
        lines.extend(configuration_lines(registry, &key.id));
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
                let capabilities = capabilities(description);
                lines.push(format!("    Capabilities: {capabilities}."));
                let mut values = Vec::new();
                if description.supports_brightness() {
                    values.push(format!(
                        "Brightness: {}",
                        light
                            .brightness_percent
                            .map_or_else(|| "unknown".into(), |value| format!("{value}%"))
                    ));
                }
                if description.supports_color_temperature() {
                    values.push(format!(
                        "temperature: {}",
                        light
                            .temperature_kelvin
                            .map_or_else(|| "unknown".into(), |value| format!("{value} K"))
                    ));
                }
                if description.supports_rgb() {
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
                if description.supports_color_temperature() || description.supports_rgb() {
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
    Context::fixed(registry, store, status)
        .handle_line(line, now)
        .await
}

impl Context<'_> {
    pub(crate) async fn handle_line(
        &self,
        line: &str,
        now: i64,
    ) -> Result<Option<String>, StorageError> {
        let view = self.view();
        let registry = &view;
        let store = &self.store;
        let status = self.status;
        let words = line.split_whitespace().collect::<Vec<_>>();
        let text = match words.as_slice() {
            [] => return Ok(None),
            ["help"] => format!("{COMMAND_HELP}\n{}", view.render_status(status, now)),
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
                        render(registry, Some(did)),
                        view.render_devices(status, now, Some(did))
                    )
                }
            }
            [verb @ ("on" | "off"), did, channel @ ..] if channel.len() <= 1 => {
                let Some(physical) = registry.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Ok(Some(invalid(registry, status, now, "Unknown device DID.")));
                };
                let function = if let Some(channel) = channel.first() {
                    physical.function(channel)
                } else if let [function] = physical.functions() {
                    Some(function.clone())
                } else {
                    return Ok(Some(invalid(
                        registry,
                        status,
                        now,
                        "A channel is required for this device.",
                    )));
                };
                let Some(function) = function else {
                    return Ok(Some(invalid(registry, status, now, "Unknown channel.")));
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

/// EOF ends input while the caller continues polling every bridge service.
pub async fn run_input(
    registry: &DeviceRegistry,
    store: &DeviceStore,
    status: &AuthStatus,
    input: impl futures_lite::io::AsyncBufRead + Unpin,
    output: impl futures_lite::io::AsyncWrite + Unpin,
) -> Result<(), RuntimeError> {
    Context::fixed(registry, store, status)
        .run_input(input, output)
        .await
}

impl Context<'_> {
    pub(crate) async fn run_input(
        &self,
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
            if let Some(result) = self.handle_line(&line, current_time()).await? {
                output.write_all(result.as_bytes()).await?;
                output.write_all(b"\n").await?;
                output.flush().await?;
            }
        }
    }
}
