//! The confirmed source facts and the serving device instances have one
//! synchronous handoff: pure facts go to planning, prepared resources are
//! consumed once by installation after the complete configuration commits.
//!
//! Preparation and installation consume the explicit plan operations; no
//! second inference of reuse, replacement or removal happens here. A
//! logical-only replacement swaps the delivery sink on the unchanged native
//! runtime, so native subscriptions and observations are never rebuilt.

use super::{
    model::AdapterError,
    state::{Context, DeviceSlot, GroupSource, LogicalSink},
    work::DeviceWork,
};
use crate::{
    configuration::{
        GroupRelation, NativeDescription, OperationKind, Plan, SourceDevice, SourceFacts,
    },
    device::{Availability, CommandReceiver, DeviceKey, PhysicalDevice},
};
use std::{collections::BTreeMap, rc::Rc};

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
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
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
            if let Some(work) = &slot.admitted {
                work.physical().reset_actions();
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
                .or_else(|| slot.admitted.as_ref().map(|work| work.native.clone()));
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
            if let Some(work) = &slot.admitted {
                entries.push(Rc::new(SourceEntry {
                    native: work.native.clone(),
                    physical: work.physical(),
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

    /// Prepare resources for the explicit plan operations. A reused native
    /// runtime is shared; a logical-only replacement prepares only the new
    /// delivery sink. Added or authority/native replacements prepare the new
    /// native runtime and its sink once.
    pub(crate) fn prepare(&self, plan: &Plan) -> PreparedSources {
        let mut installed = Vec::new();
        let mut removed = Vec::new();
        for operation in &plan.operations {
            let key = operation.key.clone();
            match operation.kind {
                OperationKind::Removed => removed.push(key),
                OperationKind::Reused => {
                    let work = self.0.live.borrow().devices[&key.id]
                        .admitted
                        .clone()
                        .expect("a reused device keeps its native runtime");
                    let sink = work.current_sink();
                    installed.push(PreparedSource {
                        key,
                        kind: operation.kind,
                        work,
                        sink,
                        receivers: None,
                    });
                }
                OperationKind::ReplacedLogical => {
                    let work = self.0.live.borrow().devices[&key.id]
                        .admitted
                        .clone()
                        .expect("a logical replacement keeps its native runtime");
                    let served = plan
                        .derived
                        .served
                        .get(&key)
                        .expect("a planned device has a served description");
                    let (physical, receivers) = PhysicalDevice::new(served.description.clone());
                    installed.push(PreparedSource {
                        key,
                        kind: operation.kind,
                        work,
                        sink: Rc::new(LogicalSink::new(physical)),
                        receivers: Some(receivers),
                    });
                }
                OperationKind::Added
                | OperationKind::ReplacedNative
                | OperationKind::ReplacedAuthority => {
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
                    let sink = Rc::new(LogicalSink::new(physical));
                    let light_write = native
                        .device
                        .functions()
                        .iter()
                        .any(|function| function.lighting().is_some())
                        .then(|| {
                            self.0
                                .light_writes
                                .for_device(&key.id, self.0.clock.clone())
                        });
                    let work = Rc::new(DeviceWork::new(
                        native.clone(),
                        sink.clone(),
                        self.0.refresh_changed.clone(),
                        light_write,
                    ));
                    installed.push(PreparedSource {
                        key,
                        kind: operation.kind,
                        work,
                        sink,
                        receivers: Some(receivers),
                    });
                }
            }
        }
        PreparedSources { installed, removed }
    }

    /// Install the committed configuration in two phases: first retire removed,
    /// replaced and logical delivery targets and install every prepared member
    /// and group, then start consumers, work, refreshes and group
    /// recomputation. The caller guarantees that no await happens between
    /// prepare, commit and install.
    pub(crate) fn install(&self, plan: &Plan, prepared: PreparedSources) -> Vec<Rc<SourceEntry>> {
        for key in &prepared.removed {
            self.retire_instance(key);
        }
        let mut started = Vec::new();
        for source in prepared.installed {
            let did = source.key.id.clone();
            match source.kind {
                OperationKind::Reused => {
                    if let Some(device) = plan.configuration.device(&source.key) {
                        source.sink.physical.set_label(device.label.clone());
                    }
                }
                OperationKind::ReplacedLogical => {
                    let old = source.work.replace_sink(source.sink.clone());
                    old.physical.set_availability(Availability::Unavailable(
                        "logical instance replaced".into(),
                    ));
                    old.close();
                    self.0.registry.remove(old.physical.key());
                    if let Some(device) = plan.configuration.device(&source.key) {
                        source.sink.physical.set_label(device.label.clone());
                    }
                    self.0.registry.insert(source.sink.physical.clone());
                }
                OperationKind::Added
                | OperationKind::ReplacedNative
                | OperationKind::ReplacedAuthority => {
                    self.retire_instance(&source.key);
                    if let Some(device) = plan.configuration.device(&source.key) {
                        source.sink.physical.set_label(device.label.clone());
                    }
                    self.0.registry.insert(source.sink.physical.clone());
                    // Binding feedback history is retired only when the real
                    // control bindings changed; authority and logical
                    // replacements keep the same DID's raw receipts.
                    if matches!(source.kind, OperationKind::ReplacedNative) {
                        self.0.light_writes.clear_authority(&did);
                    }
                    if matches!(
                        source.kind,
                        OperationKind::ReplacedNative | OperationKind::ReplacedAuthority
                    ) {
                        self.0.retire_device_notifications(&did);
                    }
                }
                OperationKind::Removed => unreachable!("removed operations are retired above"),
            }
            // Relation facts are part of the committed configuration, even
            // when the logical shape itself was reused.
            let group = plan.derived.groups.get(&did).map(group_source);
            let repair = plan
                .derived
                .served
                .get(&source.key)
                .and_then(|served| served.repair.clone())
                .map(Rc::new);
            {
                let mut live = self.0.live.borrow_mut();
                let slot: &mut DeviceSlot = live.devices.get_mut(&did).unwrap();
                slot.admitted = Some(source.work.clone());
                slot.effective_group = group;
                slot.effective_repair = repair;
                slot.candidate = None;
            }
            started.push(source);
        }
        // Every member and group is installed and the committed graph is in
        // place before any work, refresh or aggregation starts.
        let mut refreshed = Vec::new();
        for source in started {
            let did = source.key.id.clone();
            if let Some(receivers) = source.receivers {
                self.0
                    .add_consumers(source.work.clone(), source.sink.clone(), receivers);
            }
            self.0.install_work(&did);
            self.0.update_availability(&did);
            if let Some(work) = self.0.live.borrow().devices[&did].admitted.as_ref() {
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
        if let Some(work) = self
            .0
            .live
            .borrow()
            .devices
            .get(&key.id)
            .and_then(|slot| slot.admitted.as_ref())
        {
            work.physical().set_label(label.into());
        }
    }

    pub(crate) fn pending(&self, key: &DeviceKey, reason: String) {
        self.0.problem(&key.id, Some(reason));
    }

    /// Retire one serving native runtime without touching its committed identity.
    fn retire_instance(&self, key: &DeviceKey) {
        let work = self
            .0
            .live
            .borrow()
            .devices
            .get(&key.id)
            .and_then(|slot| slot.admitted.clone());
        if let Some(work) = work {
            let sink = work.current_sink();
            sink.physical
                .set_availability(Availability::Unavailable("source replaced".into()));
            work.life.close();
            sink.close();
            self.0.registry.remove(sink.physical.key());
        }
        // Drop the retired runtime from the connection attachment so the
        // replacement starts its own local subscription work.
        self.0.retire_work(&key.id);
    }

    fn serving_entries(&self) -> Vec<Rc<SourceEntry>> {
        let live = self.0.live.borrow();
        let mut entries = live
            .devices
            .values()
            .filter_map(|slot| {
                let work = slot.admitted.as_ref()?;
                Some(Rc::new(SourceEntry {
                    native: work.native.clone(),
                    physical: work.physical(),
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
        if let Some(work) = &admitted {
            let sink = work.current_sink();
            sink.physical
                .set_availability(Availability::Unavailable(reason.into()));
            sink.physical.close();
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
