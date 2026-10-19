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
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::{
    configuration::{
        CommittedConfiguration, GroupRelation, NativeDescription, OperationKind, Plan,
        SourceDevice, SourceFacts,
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
    /// The committed native metadata and control description.
    pub(crate) native: NativeDescription,
    pub(crate) physical: PhysicalDevice,
}

/// One prepared, not yet installed source instance. Its receivers are owned
/// by the application until installation consumes them.
pub(crate) struct PreparedSource {
    key: DeviceKey,
    kind: OperationKind,
    work: Rc<DeviceWork>,
    sink: Rc<LogicalSink>,
    receivers: Vec<CommandReceiver>,
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
    pub(crate) fn snapshot(&self) -> SourceFacts {
        #[cfg(test)]
        if let Some(observer) = self.0.snapshot_observer.borrow_mut().as_mut() {
            observer();
        }
        let live = self.0.live.borrow();
        let scope = live.scope.clone();
        let mut devices = Vec::new();
        for slot in live.devices.values() {
            let Some(native) = slot.latest.clone() else {
                continue;
            };
            devices.push(SourceDevice {
                key: native.device.key().clone(),
                native,
                name: slot.latest_name.clone(),
                fresh: slot.confirmed,
            });
        }
        devices.sort_by(|a, b| a.key.cmp(&b.key));
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
        SourceFacts {
            scope,
            devices,
            relations,
        }
    }

    /// Serving entries projected from one committed configuration. The native
    /// metadata comes from the committed value, never from an older runtime.
    pub(crate) fn entries(&self, configuration: &CommittedConfiguration) -> Vec<Rc<SourceEntry>> {
        let live = self.0.live.borrow();
        let mut entries = live
            .devices
            .values()
            .filter_map(|slot| {
                let work = slot.admitted.as_ref()?;
                let native = configuration
                    .device(&work.key)
                    .and_then(|device| device.native.clone())
                    .expect("an installed device has committed native facts");
                Some(Rc::new(SourceEntry {
                    native,
                    physical: work.physical(),
                }))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.physical.key().cmp(b.physical.key()));
        entries
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
                OperationKind::Reused => {}
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
                        receivers,
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
                        receivers,
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
        let changed = !prepared.installed.is_empty()
            || !prepared.removed.is_empty()
            || self.0.graph.borrow().rules != plan.derived.rules;
        for key in &prepared.removed {
            self.0.retire_work(&key.id);
        }
        let mut started = Vec::new();
        for source in prepared.installed {
            let did = source.key.id.clone();
            match source.kind {
                OperationKind::ReplacedLogical => {
                    let old = source.work.replace_sink(source.sink.clone());
                    old.physical.set_availability(Availability::Unavailable(
                        "logical instance replaced".into(),
                    ));
                    old.close();
                    self.0.registry.remove(old.physical.key());
                    self.0.registry.insert(source.sink.physical.clone());
                }
                OperationKind::Added
                | OperationKind::ReplacedNative
                | OperationKind::ReplacedAuthority => {
                    self.0.retire_work(&source.key.id);
                    self.0.registry.insert(source.sink.physical.clone());
                    // Binding feedback history is retired only when the real
                    // control bindings changed; authority and logical
                    // replacements keep the same DID's raw receipts.
                    if matches!(source.kind, OperationKind::ReplacedNative) {
                        self.0.light_writes.clear_authority(&did);
                    }
                }
                OperationKind::Removed | OperationKind::Reused => {
                    unreachable!("only new instances need prepared resources")
                }
            }
            self.0
                .live
                .borrow_mut()
                .devices
                .get_mut(&did)
                .unwrap()
                .admitted = Some(source.work.clone());
            started.push(source);
        }
        // Reused entries receive committed metadata without preparing resources
        // or refreshing their unchanged native feedback.
        for operation in &plan.operations {
            if operation.kind == OperationKind::Removed {
                continue;
            }
            let device = plan.configuration.device(&operation.key).unwrap();
            let mut live = self.0.live.borrow_mut();
            let slot = live.devices.get_mut(&operation.key.id).unwrap();
            slot.admitted
                .as_ref()
                .unwrap()
                .physical()
                .set_label(device.label.clone());
            slot.latest = device.native.clone();
            slot.latest_name = None;
            slot.group = plan.derived.groups.contains_key(&operation.key.id);
        }
        // Install every instance and the graph before starting consumers or
        // deriving group feedback. Each group is recomputed once per commit.
        *self.0.graph.borrow_mut() = plan.derived.clone();
        for source in started {
            let did = &source.key.id;
            self.0
                .add_consumers(source.work.clone(), source.sink, source.receivers);
            if source.kind != OperationKind::ReplacedLogical {
                self.0.install_work(did);
            }
            self.0.update_device_availability(did);
            source.work.report(&self.0, did);
        }
        if changed {
            for did in plan.derived.rules.keys() {
                self.0.update_device_availability(did);
            }
            self.0.refresh_all_group_states();
        }
        let entries = self.entries(&plan.configuration);
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
        self.graph
            .borrow()
            .served
            .get(&DeviceKey::new("xiaomi", did))
            .and_then(|served| served.repair.clone())
            .map(Rc::new)
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
            work.retire();
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
