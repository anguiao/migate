use super::{View, capabilities, unpublished_reason};
use crate::device::DeviceKey;

pub(in crate::terminal::bridge) fn configuration_lines(view: &View, did: &str) -> Vec<String> {
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
