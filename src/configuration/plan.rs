//! Pure planning: explicit operations, versions, rebuild cause and the
//! complete next derived graph.

use super::{
    CommittedConfiguration, CommittedDevice, CommittedFunction, DerivedConfiguration,
    OperationKind, Plan, PlanError, PlanInput, PlanOperation, RelayChannel, RelayType,
    default_device_label, import_label,
};
use crate::device::{DeviceKey, FunctionDescription, FunctionKey, FunctionKind};
use crate::xiaomi::capability::RawDescription;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Derive the final candidate once, then compare its publication through the
/// caller's pure projection of visible functions and shared battery capability.
pub(crate) fn plan<Shape: PartialEq>(
    input: PlanInput<'_>,
    shape: impl Fn(&[&FunctionDescription], bool) -> Shape,
) -> Result<Plan, PlanError> {
    let PlanInput {
        current,
        current_derived,
        mut facts,
        revoked,
        mut new_identities,
    } = input;
    let Some(scope) = facts.scope else {
        return Err(PlanError::InvalidInput("The current home scope is missing"));
    };
    facts
        .devices
        .retain(|device| !revoked.contains(&device.description.raw.key) || device.fresh);
    // Equivalent directory reordering must not change the committed configuration.
    facts
        .devices
        .sort_by(|a, b| a.description.raw.key.cmp(&b.description.raw.key));
    facts
        .relations
        .sort_by(|a, b| a.group_did.cmp(&b.group_did));
    for relation in &mut facts.relations {
        relation.members.sort();
        relation.members.dedup();
    }
    facts.split_devices.sort_by(|a, b| a.did.cmp(&b.did));
    let mut identities = current
        .devices
        .iter()
        .cloned()
        .map(|device| (device.key.clone(), device))
        .collect::<BTreeMap<_, _>>();
    let mut reserved = BTreeSet::new();
    for device in identities.values() {
        reserved.insert(device.endpoint);
        for function in &device.functions {
            reserved.insert(function.endpoint);
        }
    }
    reserved.extend(
        current
            .relay_channels
            .iter()
            .map(|channel| channel.endpoint),
    );
    let mut allocator = Allocator::new(reserved);
    let clear_revocations = facts
        .devices
        .iter()
        .filter(|source| revoked.contains(&source.description.raw.key) && source.fresh)
        .map(|source| source.description.raw.key.clone())
        .collect::<Vec<_>>();
    let mut next_devices = Vec::new();

    let mut descriptions = Vec::new();
    for source in facts.devices {
        let raw = source.description.raw;
        let Some(native) = source.description.native else {
            descriptions.push(raw);
            continue;
        };
        let key = raw.key.clone();
        let device = match identities.remove(&key) {
            Some(previous) => {
                let mut device = previous;
                for function in native.device().functions() {
                    if !device
                        .functions
                        .iter()
                        .any(|entry| entry.id == function.id())
                    {
                        let endpoint = allocator.allocate(&key)?;
                        device.functions.push(CommittedFunction {
                            id: function.id().to_owned(),
                            endpoint,
                        });
                    }
                }
                device.native = Some(native);
                device
            }
            None => {
                let unique_id = new_identities
                    .remove(&key)
                    .ok_or_else(|| PlanError::MissingIdentity { key: key.clone() })?;
                let endpoint = allocator.allocate(&key)?;
                let mut functions = Vec::new();
                for function in native.device().functions() {
                    let endpoint = allocator.allocate(&key)?;
                    functions.push(CommittedFunction {
                        id: function.id().to_owned(),
                        endpoint,
                    });
                }
                CommittedDevice {
                    key: key.clone(),
                    unique_id,
                    endpoint,
                    label: device_label(&raw),
                    configuration_version: 1,
                    functions,
                    native: Some(native),
                }
            }
        };
        next_devices.push(device);
        descriptions.push(raw);
    }

    // Identities absent from the current filtered facts lose their current
    // native facts but keep their reservation, name and version.
    for mut device in identities.into_values() {
        device.native = None;
        next_devices.push(device);
    }
    next_devices.sort_by(|a, b| a.key.cmp(&b.key));

    let temperature_bindings = if current.scope.as_ref() == Some(&scope) {
        current.temperature_bindings.clone()
    } else {
        BTreeMap::new()
    };
    let mut relay_channels = current.relay_channels.clone();
    for device in &next_devices {
        let Some(native) = &device.native else {
            continue;
        };
        let relays = native
            .device()
            .functions()
            .iter()
            .filter(|function| function.kind() == FunctionKind::Relay);
        let mut panel = None;
        for function in relays {
            let key = FunctionKey::new(device.key.clone(), function.id());
            if let Some(channel) = relay_channels
                .iter_mut()
                .find(|channel| channel.scope == scope && channel.key == key)
            {
                if let Some(mode) = facts.relay_modes.get(&key) {
                    channel.mode = Some(*mode);
                }
                continue;
            }
            let reserved_endpoint = device
                .functions
                .iter()
                .find(|item| item.id == key.id)
                .unwrap()
                .endpoint;
            let endpoint = if relay_channels
                .iter()
                .any(|channel| channel.endpoint == reserved_endpoint)
            {
                allocator.allocate(&device.key)?
            } else {
                reserved_endpoint
            };
            let (panel_label, multi_key) = panel.get_or_insert_with(|| {
                let raw = descriptions
                    .iter()
                    .find(|raw| raw.key == device.key)
                    .unwrap();
                (device_label(raw), raw.has_multiple_relay_services())
            });
            let siid = function.power().unwrap().siid;
            let label = if *multi_key {
                facts
                    .split_devices
                    .iter()
                    .find(|split| split.parent_did == device.key.id && split.siid == siid)
                    .and_then(|split| split.name.as_deref())
                    .filter(|name| !name.trim().is_empty())
                    .map(import_label)
                    .unwrap_or_else(|| relay_fallback_label(panel_label, siid))
            } else {
                panel_label.clone()
            };
            let mut hash = Sha256::new();
            for part in [&device.unique_id, &scope.uid, &scope.home_id, &key.id] {
                hash.update((part.len() as u64).to_be_bytes());
                hash.update(part.as_bytes());
            }
            let digest = hash.finalize();
            let unique_id = uuid::Uuid::from_bytes(digest[..16].try_into().unwrap())
                .simple()
                .to_string();
            relay_channels.push(RelayChannel {
                scope: scope.clone(),
                key: key.clone(),
                endpoint,
                unique_id,
                label,
                configuration_version: 1,
                mode: facts.relay_modes.get(&key).copied(),
                device_type: RelayType::Switch,
            });
        }
    }
    let mut configuration = CommittedConfiguration {
        scope: Some(scope),
        root_version: current.root_version,
        devices: next_devices,
        descriptions,
        groups: facts.relations,
        split_devices: facts.split_devices,
        relay_channels,
        temperature_bindings,
    };
    let derived = configuration.derive();
    let mut root_changed = (current.scope != configuration.scope
        && (!current_derived.published_relays.is_empty() || !derived.published_relays.is_empty()))
        || current_derived.published != derived.published
        || current_derived.published_relays != derived.published_relays;
    for channel in &mut configuration.relay_channels {
        if let Some(previous) = current
            .relay_channels
            .iter()
            .find(|old| old.scope == channel.scope && old.key == channel.key)
        {
            let old_visible = current.scope.as_ref() == Some(&channel.scope)
                && current_derived.published_relays.contains(&channel.key);
            let new_visible = configuration.scope.as_ref() == Some(&channel.scope)
                && derived.published_relays.contains(&channel.key);
            if old_visible != new_visible {
                channel.configuration_version = previous
                    .configuration_version
                    .checked_add(1)
                    .ok_or_else(|| PlanError::VersionExhausted {
                        key: channel.key.device.clone(),
                    })?;
            }
        }
    }
    for device in &mut configuration.devices {
        if current.device(&device.key).is_none() {
            continue;
        }
        let old_pub = current_derived.published.contains(&device.key);
        let new_pub = derived.published.contains(&device.key);
        let shape_changed = if old_pub && new_pub {
            let before = shape(
                &current_derived.visible_functions(&device.key),
                current_derived.served[&device.key]
                    .description
                    .has_battery(),
            );
            let after = shape(
                &derived.visible_functions(&device.key),
                derived.served[&device.key].description.has_battery(),
            );
            if before != after {
                root_changed = true;
                true
            } else {
                false
            }
        } else {
            false
        };
        if old_pub != new_pub || shape_changed {
            device.configuration_version =
                device.configuration_version.checked_add(1).ok_or_else(|| {
                    PlanError::VersionExhausted {
                        key: device.key.clone(),
                    }
                })?;
        }
    }
    if root_changed {
        configuration.root_version =
            configuration
                .root_version
                .checked_add(1)
                .ok_or(PlanError::InvalidInput(
                    "Root configuration version is exhausted",
                ))?;
    }

    let operations = operations(
        current,
        current_derived,
        &derived,
        &configuration,
        &clear_revocations,
        current.scope != configuration.scope,
    );
    let instances_changed = operations
        .iter()
        .any(|operation| !matches!(operation.kind, OperationKind::Reused));
    let rebuild = current.scope != configuration.scope
        || current_derived.published != derived.published
        || current_derived.published_relays != derived.published_relays
        || current_derived.rules != derived.rules
        || instances_changed;
    let changed = &configuration != current || !clear_revocations.is_empty();
    Ok(Plan {
        configuration,
        derived: std::rc::Rc::new(derived),
        operations,
        clear_revocations,
        rebuild,
        changed,
    })
}

fn relay_fallback_label(panel: &str, siid: u32) -> String {
    let suffix = format!(" ({siid})");
    let mut end = panel.len().min(32 - suffix.len());
    while !panel.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &panel[..end])
}

fn device_label(raw: &RawDescription) -> String {
    raw.name
        .as_deref()
        .map(import_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| default_device_label(&raw.model, &raw.key.id))
}

fn operations(
    current: &CommittedConfiguration,
    current_derived: &DerivedConfiguration,
    derived: &DerivedConfiguration,
    configuration: &CommittedConfiguration,
    clear_revocations: &[DeviceKey],
    scope_changed: bool,
) -> Vec<PlanOperation> {
    let mut operations = Vec::new();
    for device in &configuration.devices {
        let Some(after) = &device.native else {
            continue;
        };
        let kind = match current.device(&device.key) {
            None => OperationKind::Added,
            Some(previous) => match &previous.native {
                None => OperationKind::Added,
                Some(before) if !before.same_control_as(after) => OperationKind::ReplacedNative,
                Some(_) if scope_changed || clear_revocations.contains(&device.key) => {
                    OperationKind::ReplacedAuthority
                }
                Some(_) => {
                    let before = &current_derived.served[&device.key].description;
                    let after = &derived.served[&device.key].description;
                    if before.functions() != after.functions() {
                        OperationKind::ReplacedLogical
                    } else {
                        OperationKind::Reused
                    }
                }
            },
        };
        operations.push(PlanOperation {
            key: device.key.clone(),
            kind,
        });
    }
    for device in &current.devices {
        let still_served = configuration
            .device(&device.key)
            .is_some_and(|next| next.native.is_some());
        if device.native.is_some() && !still_served {
            operations.push(PlanOperation {
                key: device.key.clone(),
                kind: OperationKind::Removed,
            });
        }
    }
    operations.sort_by(|a, b| a.key.cmp(&b.key));
    operations
}

struct Allocator {
    next: u16,
}

impl Allocator {
    fn new(reserved: BTreeSet<u16>) -> Self {
        let next = reserved.iter().next_back().copied().unwrap_or(1);
        Self {
            next: next.saturating_add(1),
        }
    }

    fn allocate(&mut self, key: &DeviceKey) -> Result<u16, PlanError> {
        let endpoint = self.next;
        if endpoint > 65534 {
            return Err(PlanError::EndpointExhausted { key: key.clone() });
        }
        self.next = endpoint.saturating_add(1);
        Ok(endpoint)
    }
}
