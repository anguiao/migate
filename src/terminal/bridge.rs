use super::{AuthStatus, current_time};
use crate::{
    RuntimeError,
    device::{
        AppliedGroup, Availability, Capability, Category, Command, DeviceKey, DeviceRegistry,
        Function, FunctionSnapshot, GroupConfiguration, GroupDescription, GroupSnapshot, LightMode,
        PhysicalDevice, Power, RegistrySnapshot, Rgb,
    },
    lighting::{LightingExecutor, clock::SystemClock, group::Fanout},
    storage::{DeviceStore, StorageError},
    xiaomi::adapter::PendingDevice,
};
use std::{collections::HashMap, rc::Rc};
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

/// The pending configuration header. A recorded apply failure is part of the
/// pending context only: the applied configuration stays authoritative until
/// the candidate is committed, and a failure with nothing pending is never
/// presented as current.
fn pending_header(snapshot: &RegistrySnapshot, apply_error: Option<&str>) -> String {
    let applied = match snapshot.applied.confirmed {
        Some(version) => format!("applied version {version}."),
        None => "no committed version is applied.".into(),
    };
    let mut header = format!("Configuration: confirmed changes are pending application; {applied}");
    if let Some(error) = apply_error {
        header.push('\n');
        header.push_str(&format!(
            "Last configuration apply failed: {error}; the applied configuration is still in use, retrying."
        ));
    }
    header
}

/// One confirmed candidate group compared with the currently applied group.
/// The pending set is complete, so an applied group with no observable field
/// change is omitted instead of guessing why the candidate exists. The
/// summary names member and derived-description changes instead of repeating
/// the whole fixed description.
fn pending_group_line(
    group: &GroupConfiguration,
    applied: Option<&AppliedGroup>,
    labels: &Labels,
) -> Option<String> {
    let names = member_names(labels, &group.members);
    let capabilities = capability_list(&group.capabilities);
    let Some(applied) = applied else {
        return Some(format!(
            "+ {} ({}): new group; members: {names}; capabilities: {capabilities}.",
            group.key.id, group.label
        ));
    };
    let mut changes = Vec::new();
    if applied.members != group.members {
        let applied_names = member_names(labels, &applied.members);
        changes.push(format!("members: {names} (applied: {applied_names})"));
    }
    let applied_capabilities = applied.description().capabilities;
    if applied_capabilities != group.capabilities {
        changes.push(format!(
            "capabilities: {capabilities} (applied: {})",
            capability_list(&applied_capabilities)
        ));
    }
    if applied.label != group.label {
        changes.push(format!(
            "name: {} (applied: {})",
            group.label, applied.label
        ));
    }
    if changes.is_empty() {
        return None;
    }
    Some(format!(
        "~ {} ({}): {}.",
        group.key.id,
        group.label,
        changes.join("; ")
    ))
}

/// Render confirmed changes, optionally restricted to a selected DID. A member
/// moving between groups reports both the relation it leaves and the one it joins.
fn format_pending_configuration(
    snapshot: &RegistrySnapshot,
    labels: &Labels,
    did: Option<&str>,
    apply_error: Option<&str>,
) -> Option<String> {
    let pending = snapshot.pending.as_ref()?;
    let mut lines = vec![pending_header(snapshot, apply_error)];
    for group in &pending.groups {
        let applied = snapshot
            .applied
            .groups
            .iter()
            .find(|applied| applied.key == group.key);
        if did.is_some_and(|did| {
            group.key.id != did
                && !group.members.iter().any(|member| member.id == did)
                && !applied.is_some_and(|group| group.members.iter().any(|member| member.id == did))
        }) {
            continue;
        }
        match pending_group_line(group, applied, labels) {
            Some(line) => lines.push(format!("  {line}")),
            // A complete candidate can be waiting because another relation changed.
            None if did.is_some_and(|did| group.key.id == did) => lines.push(format!(
                "  {} ({}): relation and description unchanged.",
                group.key.id, group.label
            )),
            None => {}
        }
    }
    for applied in &snapshot.applied.groups {
        if pending.groups.iter().any(|group| group.key == applied.key)
            || did.is_some_and(|did| {
                applied.key.id != did && !applied.members.iter().any(|member| member.id == did)
            })
        {
            continue;
        }
        lines.push(format!(
            "  - {} ({}): group removed.",
            applied.key.id, applied.label
        ));
    }
    for (sign, from, to) in [
        ("+", &pending.devices, &snapshot.applied.devices),
        ("-", &snapshot.applied.devices, &pending.devices),
    ] {
        let names = member_names(
            labels,
            from.iter().map(PhysicalDevice::key).filter(|key| {
                did.is_none_or(|did| key.id == did) && !to.iter().any(|device| device.key() == *key)
            }),
        );
        if !names.is_empty() {
            lines.push(format!("  {sign} standalone: {names}."));
        }
    }
    let removed = member_names(
        labels,
        pending
            .removed
            .iter()
            .filter(|key| did.is_none_or(|did| key.id == did)),
    );
    if !removed.is_empty() {
        let suffix = if did.is_some() { "" } else { "." };
        lines.push(format!("  confirmed removed devices: {removed}{suffix}"));
    }
    (did.is_none() || lines.len() > 1).then(|| lines.join("\n"))
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
/// A terminal session sharing the physical lighting scheduler with Matter.
/// Standalone sessions drive their scheduler through `run_input`.
pub struct Terminal<'a> {
    registry: DeviceRegistry,
    store: DeviceStore,
    status: &'a AuthStatus,
    executor: Rc<LightingExecutor>,
    /// The session created the executor and therefore owns its runner.
    owned: bool,
}

impl<'a> Terminal<'a> {
    /// The production entry used by a standalone terminal session and by
    /// integration tests. The session creates and keeps the executor used by
    /// every command it handles, and [`Terminal::run_input`] drives it.
    pub fn new(registry: &DeviceRegistry, store: &DeviceStore, status: &'a AuthStatus) -> Self {
        Self {
            registry: registry.clone(),
            store: store.clone(),
            status,
            executor: LightingExecutor::new(Rc::new(SystemClock::new())),
            owned: true,
        }
    }

    /// Runtime entry: share the one executor the Matter handlers also use. The
    /// runtime runner keeps it advancing, so this session does not drive it.
    pub(crate) fn with_executor(
        registry: &DeviceRegistry,
        store: &DeviceStore,
        status: &'a AuthStatus,
        executor: Rc<LightingExecutor>,
    ) -> Self {
        Self {
            registry: registry.clone(),
            store: store.clone(),
            status,
            executor,
            owned: false,
        }
    }

    /// Drive this session's executor. The future never completes; drop it to
    /// stop the session. Callers that handle lines without [`Terminal::run_input`]
    /// must poll this alongside their input so member commands keep executing.
    pub async fn run(&self) {
        self.executor.clone().run().await
    }

    /// Handle one terminal line. A group command returns as soon as one member
    /// completely accepted; the remaining members keep executing.
    pub async fn handle_line(&self, line: &str, now: i64) -> Result<Option<String>, StorageError> {
        let words = line.split_whitespace().collect::<Vec<_>>();
        let snapshot = self.registry.snapshot();
        let apply_error = self.registry.apply_error();
        let text = match words.as_slice() {
            [] => return Ok(None),
            ["help"] => format!("{COMMAND_HELP}\n{}", self.status.render(now)),
            ["list"] => {
                let labels = labels(&self.store, &snapshot)?;
                let mut text = render_applied(&snapshot, &labels, None);
                if let Some(pending) =
                    format_pending_configuration(&snapshot, &labels, None, apply_error.as_deref())
                {
                    text.push('\n');
                    text.push_str(&pending);
                }
                for pending in self.status.pending() {
                    let name = if pending.group {
                        pending_name(&self.store, &pending)?
                    } else {
                        let key = DeviceKey::new("xiaomi", &pending.did);
                        match snapshot.devices.iter().find(|device| device.key() == &key) {
                            Some(physical) => physical.label(),
                            None => self
                                .store
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
                let labels = labels(&self.store, &snapshot)?;
                let mut text = render_applied(&snapshot, &labels, None);
                if let Some(pending) =
                    format_pending_configuration(&snapshot, &labels, None, apply_error.as_deref())
                {
                    text.push('\n');
                    text.push_str(&pending);
                }
                text.push('\n');
                text.push_str(&self.status.render_devices(now, None, &snapshot));
                text
            }
            ["status", did] => {
                let labels = labels(&self.store, &snapshot)?;
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
                let adapter_pending = self
                    .status
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
                    return Ok(Some(invalid(self.status, now, "Unknown device DID.")));
                };
                if let Some(pending) = &adapter_pending {
                    sections.push(format_pending(
                        pending,
                        &pending_name(&self.store, pending)?,
                    ));
                }
                if (applied || live.is_some())
                    && let Some(pending) = format_pending_configuration(
                        &snapshot,
                        &labels,
                        Some(did),
                        apply_error.as_deref(),
                    )
                {
                    sections.push(pending);
                }
                sections.push(if let Some(group) = applied_group {
                    let members = group
                        .members
                        .iter()
                        .map(|member| member.id.clone())
                        .collect::<Vec<_>>();
                    self.status.render_group_devices(now, &members, &snapshot)
                } else if applied || live.is_some() {
                    self.status.render_devices(now, Some(did), &snapshot)
                } else {
                    self.status.render(now)
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
                let pending_group = self
                    .status
                    .pending()
                    .into_iter()
                    .find(|pending| pending.did == *did && pending.group);
                if let Some(group) = applied_group {
                    if !channel.is_empty() {
                        return Ok(Some(invalid(
                            self.status,
                            now,
                            "A light group has no channel; select a member DID for channel control.",
                        )));
                    }
                    // The applied handle fixes the member snapshot now; every
                    // member is submitted synchronously and owned by the shared
                    // executor, so the response returns on the first complete
                    // acceptance while the remaining members keep executing.
                    let dispatch =
                        self.executor
                            .fanout(group.handle(), Fanout::Power(*verb == "on"), None);
                    return Ok(Some(match dispatch.accepted().await {
                        Ok(_) => {
                            log::info!("terminal {did} {verb}: at least one member accepted");
                            format!("{did}: at least one member accepted")
                        }
                        Err(error) => {
                            log::info!("terminal {did} {verb}: all members failed: {error}");
                            format!("{did}: all members failed: {error}")
                        }
                    }));
                }
                if candidate_group.is_some() || pending_group.is_some() {
                    return Ok(Some(invalid(
                        self.status,
                        now,
                        &format!(
                            "{did} is a light group waiting to apply; it is not published yet."
                        ),
                    )));
                }
                let Some(physical) = self.registry.get(&DeviceKey::new("xiaomi", *did)) else {
                    return Ok(Some(invalid(self.status, now, "Unknown device DID.")));
                };
                let function = if let Some(channel) = channel.first() {
                    physical.function(channel)
                } else if let [function] = physical.functions() {
                    Some(function.clone())
                } else {
                    return Ok(Some(invalid(
                        self.status,
                        now,
                        "A channel is required for this device.",
                    )));
                };
                let Some(function) = function else {
                    return Ok(Some(invalid(self.status, now, "Unknown channel.")));
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
            _ => invalid(self.status, now, "Invalid command usage."),
        };
        Ok(Some(text))
    }
    /// Process terminal input. A session that owns its executor drives it for
    /// as long as the input session is live, so group work keeps advancing
    /// between and after command lines; a runtime-injected executor is already
    /// driven by the runtime's single runner.
    pub async fn run_input(
        &self,
        input: impl futures_lite::io::AsyncBufRead + Unpin,
        output: impl futures_lite::io::AsyncWrite + Unpin,
    ) -> Result<(), RuntimeError> {
        if !self.owned {
            return self.input_loop(input, output).await;
        }
        futures_lite::future::or(
            async {
                self.run().await;
                Ok::<(), RuntimeError>(())
            },
            self.input_loop(input, output),
        )
        .await
    }

    /// EOF ends input while the caller continues polling every bridge service.
    async fn input_loop(
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

#[cfg(test)]
mod tests;
