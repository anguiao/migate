//! Pure planning: explicit operations, versions, rebuild cause and the
//! complete next derived graph.

use super::{
    CommittedConfiguration, CommittedDevice, CommittedFunction, DerivedConfiguration,
    OperationKind, Plan, PlanError, PlanInput, PlanOperation, device_label,
    same_control_description,
};
use crate::device::DeviceKey;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn plan(input: PlanInput) -> Result<Plan, PlanError> {
    let PlanInput {
        current,
        current_derived,
        current_shapes,
        mut facts,
        facts_shapes,
        revocations,
        mut new_identities,
    } = input;
    let Some(scope) = facts.scope.clone() else {
        return Err(PlanError::InvalidInput("The current home scope is missing"));
    };
    facts.devices.sort_by(|a, b| a.key.cmp(&b.key));
    facts
        .relations
        .sort_by(|a, b| a.group_did.cmp(&b.group_did));
    let revoked = revocations
        .into_iter()
        .map(|revocation| revocation.key)
        .collect::<BTreeSet<_>>();
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

    for source in &facts.devices {
        let key = source.key.clone();
        // The application filters non-fresh revoked facts before planning;
        // any revoked fact here is a genuine fresh re-admission.
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
                if source.native.device().functions().len() > 1
                    || source
                        .native
                        .device()
                        .functions()
                        .iter()
                        .any(|function| function.kind().is_measurement())
                {
                    for function in &mut device.functions {
                        if function.endpoint == device.endpoint {
                            function.endpoint = allocator.allocate(&key)?;
                        }
                    }
                }
                device.native = Some(source.native.clone());
                device
            }
            None => {
                let unique_id = new_identities
                    .remove(&key)
                    .ok_or_else(|| PlanError::MissingIdentity { key: key.clone() })?;
                let single = source.native.device().functions().len() == 1
                    && !source.native.device().functions()[0]
                        .kind()
                        .is_measurement();
                let endpoint = allocator.allocate(&key)?;
                let mut functions = Vec::new();
                for function in source.native.device().functions() {
                    let endpoint = if single {
                        endpoint
                    } else {
                        allocator.allocate(&key)?
                    };
                    functions.push(CommittedFunction {
                        id: function.id().to_owned(),
                        endpoint,
                    });
                }
                CommittedDevice {
                    key: key.clone(),
                    unique_id,
                    endpoint,
                    label: device_label(source),
                    configuration_version: 1,
                    functions,
                    native: Some(source.native.clone()),
                }
            }
        };
        next_devices.push(device);
    }

    // Identities absent from the current filtered facts lose their current
    // native facts but keep their reservation, name and version.
    for device in identities.values_mut() {
        device.native = None;
        next_devices.push(device.clone());
    }
    next_devices.sort_by(|a, b| a.key.cmp(&b.key));

    // Group facts use a canonical member order so an equivalent directory
    // reordering never changes the committed configuration or rebuilds.
    let groups = facts
        .relations
        .iter()
        .map(|relation| {
            let mut relation = relation.clone();
            relation.members.sort();
            relation.members.dedup();
            relation
        })
        .collect::<Vec<_>>();
    let mut configuration = CommittedConfiguration {
        scope: Some(scope),
        root_version: current.root_version,
        devices: next_devices,
        groups,
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
            let before = current_shapes
                .get(&device.key)
                .ok_or(PlanError::InvalidInput(
                    "A committed Matter shape is missing",
                ))?;
            let after = facts_shapes
                .get(&device.key)
                .ok_or(PlanError::InvalidInput("A source Matter shape is missing"))?;
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
        &current,
        &current_derived,
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
    let changed = configuration != current || !clear_revocations.is_empty();
    Ok(Plan {
        configuration,
        derived: std::rc::Rc::new(derived),
        operations,
        clear_revocations,
        rebuild,
        changed,
    })
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
