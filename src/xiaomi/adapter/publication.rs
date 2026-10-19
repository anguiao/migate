//! The confirmed source facts and the serving device instances have one
//! synchronous handoff: pure facts go to planning, prepared resources are
//! consumed once by installation after the complete configuration commits.
//!
//! Preparation and installation consume the explicit plan operations; no
//! second inference of reuse, replacement or removal happens here. The
//! source-internal immutable-object handoff is completed by a later slice.

use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, DeviceSlot, GroupSource, Lifetime},
};
use crate::{
    configuration::{
        GroupRelation, NativeDescription, OperationKind, Plan, SourceDevice, SourceFacts,
    },
    device::{Availability, CommandReceiver, DeviceKey, PhysicalDevice},
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

/// The live Xiaomi source continues running while Matter instances restart.
#[derive(Clone)]
pub struct SourceHandle(pub(super) Rc<Context>);

/// One serving source device as seen by the terminal and diagnostics. It never
/// exposes prepared receivers or configuration submission.
pub(crate) struct SourceEntry {
    pub(crate) native: NativeDescription,
    pub(crate) physical: PhysicalDevice,
    pub(crate) problem: Option<String>,
}

/// The latest pure source facts and the currently serving entries.
pub(crate) struct SourceSnapshot {
    pub(crate) facts: SourceFacts,
    pub(crate) entries: Vec<Rc<SourceEntry>>,
}

/// One prepared, not yet installed source instance. Its receivers are owned
/// by the application until installation consumes them.
pub(crate) struct PreparedSource {
    key: DeviceKey,
    kind: OperationKind,
    native: NativeDescription,
    record: Rc<DeviceRecord>,
    physical: PhysicalDevice,
    receivers: Option<Vec<CommandReceiver>>,
}

/// Explicit prepared resources for the planned operations.
pub(crate) struct PreparedSources {
    installed: Vec<PreparedSource>,
    removed: Vec<DeviceKey>,
}

impl SourceHandle {
    pub(crate) fn listen(&self) -> event_listener::EventListener {
        self.0.configuration_changed.listen()
    }

    /// End every serving action and retire unsent repaired-group frames.
    pub(crate) fn stop_actions(&self) {
        let live = self.0.live.borrow();
        for slot in live.devices.values() {
            if let Some(record) = &slot.admitted {
                record.physical.reset_actions();
            }
            if let Some(work) = &slot.work {
                work.light_frames.borrow_mut().cancel_all();
            }
        }
    }

    /// Collect the latest pure source facts and the serving entries. No
    /// running device is created here.
    pub(crate) fn snapshot(&self) -> SourceSnapshot {
        #[cfg(test)]
        if let Some(observer) = self.0.snapshot_observer.borrow_mut().as_mut() {
            observer();
        }
        let live = self.0.live.borrow();
        let scope = live.scope.clone();
        let mut devices = Vec::new();
        let mut entries = Vec::new();
        for slot in live.devices.values() {
            let native = slot
                .native_confirmed
                .clone()
                .or_else(|| {
                    slot.candidate
                        .as_ref()
                        .map(|(candidate, _)| candidate.clone())
                })
                .or_else(|| {
                    slot.admitted
                        .as_ref()
                        .map(|record| record.cached.borrow().clone())
                });
            let Some(native) = native else {
                continue;
            };
            let name = slot.candidate.as_ref().and_then(|(_, name)| name.clone());
            devices.push(SourceDevice {
                key: native.device.key().clone(),
                native,
                name,
                fresh: slot.confirmed,
            });
            if let Some(record) = &slot.admitted {
                entries.push(Rc::new(SourceEntry {
                    native: record.cached.borrow().clone(),
                    physical: record.physical.clone(),
                    problem: slot.blocked.clone(),
                }));
            }
        }
        devices.sort_by(|a, b| a.key.cmp(&b.key));
        entries.sort_by(|a, b| a.physical.key().cmp(b.physical.key()));
        let problems = live
            .devices
            .iter()
            .filter_map(|(did, slot)| slot.blocked.clone().map(|problem| (did.clone(), problem)))
            .collect::<BTreeMap<_, _>>();
        drop(live);
        let relations = self
            .0
            .relations
            .borrow()
            .snapshot()
            .into_iter()
            .filter(|group| scope.as_ref() == Some(&group.scope))
            .map(|group| {
                let errors = group
                    .error
                    .into_iter()
                    .chain(problems.get(&group.membership.group_did).cloned())
                    .collect::<Vec<_>>();
                let error = (!errors.is_empty()).then(|| errors.join("; "));
                GroupRelation {
                    group_did: group.membership.group_did,
                    record_did: group.record_did,
                    model: group.model,
                    instance_type: group.instance_type,
                    is_physical: group.membership.is_physical,
                    active: group.active,
                    error,
                    members: group.membership.members,
                }
            })
            .collect();
        SourceSnapshot {
            facts: SourceFacts {
                scope,
                devices,
                relations,
            },
            entries,
        }
    }

    /// Prepare resources for the explicit plan operations. Reused instances
    /// are shared; added and replaced ones own their one-time receivers until
    /// installation.
    pub(crate) fn prepare(&self, plan: &Plan) -> PreparedSources {
        let mut installed = Vec::new();
        let mut removed = Vec::new();
        for operation in &plan.operations {
            let key = operation.key.clone();
            match operation.kind {
                OperationKind::Removed => removed.push(key),
                OperationKind::Reused => {
                    let record = self.0.live.borrow().devices[&key.id]
                        .admitted
                        .clone()
                        .expect("a reused device keeps its admitted instance");
                    let native = plan
                        .configuration
                        .device(&key)
                        .and_then(|device| device.native.clone())
                        .expect("a reused device keeps its native facts");
                    let physical = record.physical.clone();
                    installed.push(PreparedSource {
                        key,
                        kind: operation.kind,
                        native,
                        record,
                        physical,
                        receivers: None,
                    });
                }
                OperationKind::Added
                | OperationKind::ReplacedNative
                | OperationKind::ReplacedAuthority
                | OperationKind::ReplacedLogical => {
                    let native = plan
                        .configuration
                        .device(&key)
                        .and_then(|device| device.native.clone())
                        .expect("a planned device keeps its native facts");
                    let served = plan
                        .derived
                        .served
                        .get(&key)
                        .expect("a planned device has a served description");
                    let (physical, receivers) = PhysicalDevice::new(served.description.clone());
                    let record = Rc::new(DeviceRecord {
                        life: Lifetime::default(),
                        binding_generation: Cell::new(0),
                        cached: RefCell::new(native.clone()),
                        physical: physical.clone(),
                    });
                    installed.push(PreparedSource {
                        key,
                        kind: operation.kind,
                        native,
                        record,
                        physical,
                        receivers: Some(receivers),
                    });
                }
            }
        }
        PreparedSources { installed, removed }
    }

    /// Install the committed configuration in two phases: first retire removed
    /// and replaced instances and install every prepared member and group, then
    /// start consumers, work, refreshes and group recomputation. The caller
    /// guarantees that no await happens between prepare, commit and install.
    pub(crate) fn install(&self, plan: &Plan, prepared: PreparedSources) -> Vec<Rc<SourceEntry>> {
        for key in &prepared.removed {
            self.retire_instance(key);
        }
        let mut started = Vec::new();
        for source in prepared.installed {
            let did = source.key.id.clone();
            let replaced = !matches!(source.kind, OperationKind::Reused);
            if replaced {
                self.retire_instance(&source.key);
            }
            *source.record.cached.borrow_mut() = source.native.clone();
            if let Some(device) = plan.configuration.device(&source.key) {
                source.physical.set_label(device.label.clone());
            }
            let group = plan.derived.groups.get(&did).map(group_source);
            {
                let mut live = self.0.live.borrow_mut();
                let slot: &mut DeviceSlot = live.devices.get_mut(&did).unwrap();
                slot.admitted = Some(source.record.clone());
                slot.effective_group = group;
                slot.effective_repair = plan
                    .derived
                    .served
                    .get(&source.key)
                    .and_then(|served| served.repair.clone())
                    .map(Rc::new);
                slot.candidate = None;
            }
            self.0.registry.insert(source.physical.clone());
            if replaced {
                self.0.retire_device_notifications(&did);
                self.0.retire_work(&did);
                // Binding feedback history is retired only when the real
                // control bindings changed; an added or logical-only
                // replacement keeps the same DID's raw receipts.
                if matches!(source.kind, OperationKind::ReplacedNative) {
                    self.0.light_writes.clear_authority(&did);
                }
            }
            started.push(source);
        }
        // Every member and group is installed and the committed graph is in
        // place before any work, refresh or aggregation starts.
        let mut refreshed = Vec::new();
        for source in started {
            let did = source.key.id.clone();
            if let Some(receivers) = source.receivers {
                self.0.add_consumers(source.record.clone(), receivers);
            }
            self.0.install_work(&did);
            self.0.update_availability(&did);
            if let Some(work) = self.0.live.borrow().devices[&did].work.as_ref() {
                work.report(&self.0, &did);
            }
            refreshed.push(did);
        }
        for did in refreshed {
            self.0.refresh_source(&did);
        }
        for did in self
            .0
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            self.0.refresh_group_states(&did);
        }
        let entries = self.serving_entries();
        self.0.changed.notify(usize::MAX);
        entries
    }

    pub(crate) fn set_label(&self, key: &DeviceKey, label: &str) {
        if let Some(record) = self
            .0
            .live
            .borrow()
            .devices
            .get(&key.id)
            .and_then(|slot| slot.admitted.as_ref())
        {
            record.physical.set_label(label.into());
        }
    }

    pub(crate) fn pending(&self, key: &DeviceKey, reason: String) {
        self.0.problem(&key.id, Some(reason));
    }

    /// Retire one serving instance without touching its committed identity.
    fn retire_instance(&self, key: &DeviceKey) {
        let admitted = self
            .0
            .live
            .borrow()
            .devices
            .get(&key.id)
            .and_then(|slot| slot.admitted.clone());
        if let Some(record) = admitted {
            record
                .physical
                .set_availability(Availability::Unavailable("source replaced".into()));
            record.life.close();
            self.0.registry.remove(record.physical.key());
        }
    }

    fn serving_entries(&self) -> Vec<Rc<SourceEntry>> {
        let live = self.0.live.borrow();
        let mut entries = live
            .devices
            .values()
            .filter_map(|slot| {
                let record = slot.admitted.as_ref()?;
                Some(Rc::new(SourceEntry {
                    native: record.cached.borrow().clone(),
                    physical: record.physical.clone(),
                    problem: slot.blocked.clone(),
                }))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.physical.key().cmp(b.physical.key()));
        entries
    }
}

fn group_source(relation: &GroupRelation) -> GroupSource {
    GroupSource {
        record_did: relation.record_did.clone(),
        model: relation.model.clone(),
        instance_type: relation.instance_type.clone(),
        members: relation.members.clone(),
        is_physical: relation.is_physical,
    }
}

impl Context {
    pub(super) fn stage(&self) {
        self.configuration_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }

    /// The committed repair plan for one group; never a prepared candidate.
    pub(super) fn group_repair(
        &self,
        did: &str,
    ) -> Option<Rc<crate::xiaomi::capability::RepairPlan>> {
        self.live
            .borrow()
            .devices
            .get(did)?
            .effective_repair
            .clone()
    }

    /// Confirmed removal closes every retained authority before persistence.
    pub(super) fn revoke(&self, did: &str, reason: &str) -> Result<(), AdapterError> {
        let key = DeviceKey::new("xiaomi", did);
        let admitted = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        if let Some(record) = &admitted {
            record
                .physical
                .set_availability(Availability::Unavailable(reason.into()));
            record.physical.close();
        }
        if admitted.is_some() {
            self.store
                .devices()
                .record_revocations(&[crate::configuration::Revocation {
                    key,
                    reason: reason.into(),
                }])?;
        }
        self.refresh_group_states(did);
        Ok(())
    }
}
