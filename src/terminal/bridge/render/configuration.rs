use super::{
    View,
    diagnostics::FunctionDiagnostics,
    primitives::{capabilities, power_text},
};
use crate::{
    configuration::GroupRelation,
    device::{DeviceKey, FunctionDescription, FunctionKey, Power, RelayMode},
    terminal::status::{property_diagnostics, property_label},
    xiaomi::adapter::PropertyRole,
};

pub(in crate::terminal::bridge) fn configuration_lines(view: &View, did: &str) -> Vec<String> {
    let current = &view.current;
    let key = DeviceKey::new("xiaomi", did);
    let mut lines = Vec::new();
    if let Some(source) = view.description(&key)
        && let Some(diagnostic) = &source.diagnostic
    {
        lines.push(format!(
            "  Source {} ({}): recognition issue: {diagnostic}.",
            source.model, source.instance_type
        ));
    }
    if let Some(record) = current
        .configuration
        .device(&key)
        .filter(|_| view.access.descriptions.contains(&key))
    {
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
            lines.push(format!("  Category: {}.", recognition.category()));
            for function in device.functions() {
                let role = if recognition.core().iter().any(|id| id == function.id()) {
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
            for patch in recognition.patches() {
                lines.push(format!(
                    "  Patch {}: {} ({}).",
                    patch.id(),
                    patch.target(),
                    patch.stage()
                ));
            }
            for issue in recognition.diagnostics() {
                lines.push(format!(
                    "  Recognition issue {}: {}.",
                    issue.target(),
                    issue.reason()
                ));
            }
            if let Some(profile) = native.legacy_profile() {
                let patch = profile.patch();
                lines.push(format!(
                    "  Patch {}: {} ({}).",
                    patch.id(),
                    patch.target(),
                    patch.stage()
                ));
            }
        }
    }
    // The terminal lists every confirmed membership from the effective
    // relations, including groups that are currently unpublished.
    for relation in current.derived.groups.values().filter(|relation| {
        view.access.groups.contains(&relation.group_did)
            && relation.members.iter().any(|member| member == did)
    }) {
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
    if let Some(group) = current
        .derived
        .groups
        .get(did)
        .filter(|_| view.access.groups.contains(did))
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

pub(super) fn unpublished_reason(relation: &GroupRelation) -> String {
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

pub(super) fn relay_lines(
    view: &View,
    key: &FunctionKey,
    description: &FunctionDescription,
    power: Power,
    mode: Option<RelayMode>,
    diagnostics: &FunctionDiagnostics,
    show_commands: bool,
) -> Vec<String> {
    let id = &key.id;
    let label = &description.label;
    let mut lines = Vec::new();
    let channel = view
        .current
        .configuration
        .relay_channels
        .iter()
        .find(|channel| {
            view.current.configuration.scope.as_ref() == Some(&channel.scope) && channel.key == *key
        });
    if let Some(channel) = channel {
        let mode_text = match mode {
            Some(crate::device::RelayMode::Wired) => "wired".to_owned(),
            Some(crate::device::RelayMode::Wireless) => "wireless".to_owned(),
            None => match channel.mode {
                Some(crate::device::RelayMode::Wired) => "unknown (saved wired)".to_owned(),
                Some(crate::device::RelayMode::Wireless) => "unknown (saved wireless)".to_owned(),
                None => "unknown".to_owned(),
            },
        };
        let published = view.current.derived.published_relays.contains(key);
        let visibility = if !view.access.configurable_devices.contains(&key.device) {
            if published {
                "published (mapping access unavailable)"
            } else {
                "hidden (current access unavailable)"
            }
        } else {
            match (published, mode, channel.mode) {
                (true, Some(crate::device::RelayMode::Wireless), _) => {
                    "published (wireless observed; update pending)"
                }
                (true, None, _) => "published (saved mode; awaiting current reading)",
                (true, _, _) => "published",
                (false, Some(crate::device::RelayMode::Wireless), _) => "hidden (wireless mode)",
                (false, Some(crate::device::RelayMode::Wired), _) => "hidden (publication pending)",
                (false, None, Some(crate::device::RelayMode::Wireless)) => {
                    "hidden (saved wireless mode; awaiting current reading)"
                }
                (false, None, _) => "hidden (awaiting mode reading)",
            }
        };
        lines.push(format!(
            "  {id} ({}): {}; mode {mode_text}; mapping {}; Matter {visibility}; endpoint {}.",
            channel.label,
            power_text(power),
            channel.device_type.as_str(),
            channel.endpoint
        ));
        if !show_commands {
            lines.push(format!(
                "    Matter identity: unique ID {}; configuration version {}.",
                channel.unique_id, channel.configuration_version
            ));
            let native = view
                .current
                .entries
                .iter()
                .find(|entry| entry.physical.key() == &key.device)
                .and_then(|entry| {
                    entry
                        .native
                        .device()
                        .functions()
                        .iter()
                        .find(|native| native.id() == id)
                })
                .expect("a serving relay has native bindings");
            let on = native.power().unwrap();
            let (mode_binding, notify) = native.relay_mode().unwrap();
            lines.push(format!(
                "    Bindings: on {}.{} (read/write/notify); mode {}.{} (read{}).",
                on.siid,
                on.piid,
                mode_binding.siid,
                mode_binding.piid,
                if notify { "/notify" } else { "" }
            ));
            lines.push(format!(
                "    Synchronization: {}.",
                diagnostics.synchronization
            ));
            for role in [PropertyRole::Power, PropertyRole::Mode] {
                let diagnostic = property_diagnostics(diagnostics.property(role));
                lines.push(format!("    {}: {diagnostic}.", property_label(role)));
            }
        }
    } else {
        lines.push(format!(
            "  {id} ({label}): {}; awaiting relay configuration.",
            power_text(power)
        ));
    }
    lines
}
