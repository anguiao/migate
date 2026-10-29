//! Pure planning: explicit operations, versions, rebuild cause and the
//! complete next derived graph.

use super::{
    CommittedConfiguration, CommittedDevice, CommittedFunction, DerivedConfiguration,
    OperationKind, Plan, PlanError, PlanInput, PlanOperation, SourceDevice, default_device_label,
    import_label, same_control_description,
};
use crate::device::{DeviceKey, FunctionDescription};
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
        revocations,
        mut new_identities,
    } = input;
    let Some(scope) = facts.scope else {
        return Err(PlanError::InvalidInput("The current home scope is missing"));
    };
    let revoked = revocations
        .into_iter()
        .map(|revocation| revocation.key)
        .collect::<BTreeSet<_>>();
    facts
        .devices
        .retain(|device| !revoked.contains(&device.key) || device.fresh);
    // Equivalent directory reordering must not change the committed configuration.
    facts.devices.sort_by(|a, b| a.key.cmp(&b.key));
    facts
        .relations
        .sort_by(|a, b| a.group_did.cmp(&b.group_did));
    for relation in &mut facts.relations {
        relation.members.sort();
        relation.members.dedup();
    }
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
    let mut allocator = Allocator::new(reserved);
    let mut clear_revocations = Vec::new();
    let mut next_devices = Vec::new();

    for source in facts.devices {
        let key = source.key.clone();
        // Filtering precedes candidate construction, so any revoked fact here
        // is a genuine fresh re-admission.
        if revoked.contains(&key) {
            clear_revocations.push(key.clone());
        }
        let device = match identities.remove(&key) {
            Some(previous) => {
                let mut device = previous;
                for function in source.native.device().functions() {
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
                device.native = Some(source.native);
                device
            }
            None => {
                let unique_id = new_identities
                    .remove(&key)
                    .ok_or_else(|| PlanError::MissingIdentity { key: key.clone() })?;
                let endpoint = allocator.allocate(&key)?;
                let mut functions = Vec::new();
                for function in source.native.device().functions() {
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
                    label: device_label(&source),
                    configuration_version: 1,
                    functions,
                    native: Some(source.native),
                }
            }
        };
        next_devices.push(device);
    }

    // Identities absent from the current filtered facts lose their current
    // native facts but keep their reservation, name and version.
    for mut device in identities.into_values() {
        device.native = None;
        next_devices.push(device);
    }
    next_devices.sort_by(|a, b| a.key.cmp(&b.key));

    let mut configuration = CommittedConfiguration {
        scope: Some(scope),
        root_version: current.root_version,
        devices: next_devices,
        groups: facts.relations,
    };
    let derived = configuration.derive();
    let mut root_changed = current_derived.published != derived.published;
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

fn device_label(device: &SourceDevice) -> String {
    device
        .name
        .as_deref()
        .map(import_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| default_device_label(device.native.model(), &device.key.id))
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
                Some(before) if !same_control_description(before, after) => {
                    OperationKind::ReplacedNative
                }
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
