//! The confirmed source facts and the serving device instances have one
//! synchronous handoff: pure facts go to planning, prepared resources are
//! consumed once by installation after the complete configuration commits.
//!
//! Preparation and installation consume the explicit plan operations; no
//! second inference of reuse, replacement or removal happens here. A
//! logical-only replacement swaps the delivery sink on the unchanged native
//! runtime, so native subscriptions and observations are never rebuilt.

use super::{
    directory::{DescriptionOperation, DeviceSlot, Directory},
    model::AdapterError,
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::xiaomi::capability::NativeDescription;
use crate::{
    configuration::{
        CommittedConfiguration, DerivedConfiguration, GroupRelation, OperationKind, Plan,
        PlanOperation, Scope, SourceDevice, SourceFacts,
    },
    device::{Availability, CommandReceiver, DeviceKey, FunctionKey, PhysicalDevice, RelayMode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

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

impl Directory {
    fn committed_work(&self, scope: &Scope, key: &DeviceKey) -> Option<(&DeviceSlot, &DeviceWork)> {
        let slot = self.devices.get(&key.id)?;
        let work = slot.work.as_deref()?;
        (self.scope.as_ref() == Some(scope)
            && key.source == "xiaomi"
            && work.key == *key
            && !work.life.is_closed())
        .then_some((slot, work))
    }
}

impl DeviceSlot {
    fn confirmed_access(&self, hub_access: bool) -> bool {
        self.confirmed && ((self.cloud_access.is_some() && !self.cloud_forbidden) || hub_access)
    }
}

impl SourceHandle {
    pub(crate) fn configuration(&self) -> Rc<CommittedConfiguration> {
        self.0.configuration.borrow().clone()
    }

    /// Share the application's committed configuration and derived descriptions.
    pub(crate) fn set_configuration(
        &self,
        configuration: Rc<CommittedConfiguration>,
        derived: Rc<DerivedConfiguration>,
    ) {
        *self.0.configuration.borrow_mut() = configuration;
        *self.0.graph.borrow_mut() = derived;
    }

    pub(crate) fn accessible_groups(&self, scope: &Option<Scope>) -> BTreeSet<String> {
        let directory = self.0.directory.borrow();
        directory
            .confirmed_groups()
            .into_iter()
            .filter(|group| scope.as_ref() == Some(&group.scope) && directory.scope == *scope)
            .map(|group| group.membership.group_did)
            .collect()
    }

    pub(crate) fn has_scope(&self) -> bool {
        self.0.directory.borrow().scope.is_some()
    }

    /// Capture current diagnostics without waiting for the publication callback.
    pub(crate) fn diagnostics(&self) -> super::AdapterStatus {
        self.0.diagnostics()
    }

    /// Whether every initial observation needed by the committed publication has a result.
    pub(crate) fn initial_reads_ready(&self, configuration: &CommittedConfiguration) -> bool {
        super::initial_reads::ready(&self.0, configuration)
    }

    /// Register before checking `initial_reads_ready` to avoid missing an update.
    pub(crate) fn listen_initial_reads(&self) -> event_listener::EventListener {
        self.0.initial_reads_changed.listen()
    }

    /// Register before collecting source facts; confirmed relay mode changes
    /// also wake consumers even when the native description is unchanged.
    pub(crate) fn listen(&self) -> event_listener::EventListener {
        self.0.configuration_changed.listen()
    }

    /// End every serving action and retire unsent repaired-group frames.
    pub(crate) fn stop_actions(&self) {
        let live = self.0.directory.borrow();
        for slot in live.devices.values() {
            if let Some(work) = &slot.work {
                work.physical().reset_actions();
                work.light_frames.borrow_mut().cancel_all();
            }
        }
    }

    /// Retained selected-home sources, including descriptions with no runtime.
    pub(crate) fn accessible_descriptions(
        &self,
        configuration: &CommittedConfiguration,
    ) -> BTreeSet<DeviceKey> {
        let directory = self.0.directory.borrow();
        if directory.scope != configuration.scope {
            return BTreeSet::new();
        }
        directory
            .devices
            .iter()
            .filter(|(_, slot)| slot.work.as_ref().is_none_or(|work| !work.life.is_closed()))
            .map(|(did, _)| DeviceKey::new("xiaomi", did))
            .chain(
                directory
                    .split_devices()
                    .iter()
                    .map(|split| DeviceKey::new("xiaomi", &split.did)),
            )
            .collect()
    }

    /// Binding requires confirmed membership and unrevoked source authority,
    /// but does not require the device to be online.
    pub(crate) fn binding_accessible(&self, scope: &Option<Scope>, key: &DeviceKey) -> bool {
        let live = self.0.directory.borrow();
        scope
            .as_ref()
            .and_then(|scope| live.committed_work(scope, key))
            .is_some_and(|(slot, _)| {
                slot.confirmed_access(self.0.hub.borrow().access(&key.id).is_some_and(|access| {
                    access.as_ref().is_ok_and(|access| access.spec_v2_access)
                }))
            })
    }

    /// Publication supplies the last confirmed wired mode until fresh feedback arrives.
    pub(crate) fn relay_accessible(&self, scope: &Scope, key: &FunctionKey) -> bool {
        if !self.0.graph.borrow().published_relays.contains(key) {
            return false;
        }
        let live = self.0.directory.borrow();
        let Some((slot, work)) = live.committed_work(scope, &key.device) else {
            return false;
        };
        // The current home's hub can admit an already published relay while
        // the first cloud directory sync is still pending.
        let authorized = slot.confirmed_access(
            self.0
                .hub
                .borrow()
                .access(&key.device.id)
                .is_some_and(|access| access.as_ref().is_ok_and(|access| access.spec_v2_access)),
        ) || !self.0.lan_scope_invalid.get()
            && self.0.local_candidate(&key.device.id, false).is_ok();
        authorized
            && work.physical().function(&key.id).is_some_and(|function| {
                function.snapshot().relay_mode() != Some(RelayMode::Wireless)
            })
    }

    pub(crate) fn committed_graph(&self) -> Rc<crate::configuration::DerivedConfiguration> {
        self.0.graph.borrow().clone()
    }

    /// A new Matter instance must not restore reports for a source whose
    /// authority already ended, even if the next configuration failed to save.
    pub(crate) fn source_retained(&self, scope: &Option<Scope>, key: &DeviceKey) -> bool {
        let live = self.0.directory.borrow();
        live.scope == *scope
            && live
                .devices
                .get(&key.id)
                .and_then(|slot| slot.work.as_ref())
                .is_some_and(|work| work.key == *key && !work.life.is_closed())
    }

    /// Collect the latest pure source facts and the serving entries. No
    /// running device is created here.
    pub(crate) fn snapshot(&self, current: &CommittedConfiguration) -> SourceFacts {
        let directory = self.0.directory.borrow();
        let scope = directory.scope.clone();
        let same_scope = current.scope == scope;
        // A configured target awaiting a complete matching description holds
        // the whole batch. Withdrawn sources are absent and never held back.
        let waiting = same_scope
            && current.descriptions.iter().any(|committed| {
                let did = &committed.key.id;
                let Some(slot) = directory.devices.get(did) else {
                    return false;
                };
                directory
                    .group(did)
                    .is_some_and(|group| group.instance_type.is_none())
                    || directory
                        .description_target(did)
                        .as_ref()
                        .is_some_and(|(model, urn)| {
                            slot.latest_description.as_ref().is_none_or(|description| {
                                description.raw.model != *model
                                    || description.raw.instance_type != *urn
                            })
                        })
            });
        let committed = waiting.then(|| {
            current
                .source_descriptions()
                .map(|description| (description.raw.key.id.clone(), description))
                .collect::<BTreeMap<_, _>>()
        });
        let devices = directory
            .devices
            .iter()
            .filter_map(|(did, slot)| {
                let description = match &committed {
                    Some(committed) => committed.get(did),
                    None => slot.latest_description.as_ref(),
                }?;
                Some(SourceDevice {
                    description: description.clone(),
                    fresh: slot.confirmed,
                })
            })
            .collect();
        let mut relay_modes = BTreeMap::new();
        for work in directory
            .devices
            .values()
            .filter_map(|slot| slot.work.as_ref())
        {
            for function in work.physical().functions() {
                if let Some(mode) = function.snapshot().relay_mode() {
                    relay_modes.insert(function.function_key().clone(), mode);
                }
            }
        }
        let relations = directory
            .confirmed_groups()
            .into_iter()
            .filter(|group| scope.as_ref() == Some(&group.scope))
            .filter_map(|group| {
                let did = &group.membership.group_did;
                if waiting && group.active {
                    return current
                        .groups
                        .iter()
                        .find(|relation| &relation.group_did == did)
                        .cloned();
                }
                if waiting
                    && !current
                        .groups
                        .iter()
                        .any(|relation| &relation.group_did == did)
                {
                    return None;
                }
                let errors = group
                    .error
                    .into_iter()
                    .chain(directory.devices.get(did).into_iter().flat_map(|slot| {
                        slot.description_problems
                            .iter()
                            .filter(|problem| problem.operation == DescriptionOperation::Load)
                            .map(ToString::to_string)
                    }))
                    .collect::<Vec<_>>();
                Some(GroupRelation {
                    group_did: group.membership.group_did,
                    record_did: group.record_did,
                    model: group.model,
                    instance_type: group.instance_type,
                    is_physical: group.membership.is_physical,
                    active: group.active,
                    error: (!errors.is_empty()).then(|| errors.join("; ")),
                    members: group.membership.members,
                })
            })
            .collect();
        SourceFacts {
            scope,
            devices,
            relations,
            split_devices: directory.split_devices().to_vec(),
            relay_modes,
        }
    }

    /// Serving entries projected from one committed configuration. The native
    /// metadata comes from the committed value, never from an older runtime.
    pub(crate) fn entries(&self, configuration: &CommittedConfiguration) -> Vec<Rc<SourceEntry>> {
        let live = self.0.directory.borrow();
        let mut entries = live
            .devices
            .values()
            .filter_map(|slot| {
                let work = slot.work.as_ref()?;
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
                    let work = self.0.directory.borrow().devices[&key.id]
                        .work
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
                        .device()
                        .functions()
                        .iter()
                        .any(|function| function.lighting().is_some())
                        .then(|| self.0.write_gates.for_device(&key.id, self.0.clock.clone()));
                    let work = Rc::new(DeviceWork::new(
                        native.clone(),
                        sink.clone(),
                        self.0.refresh_changed.clone(),
                        self.0.initial_reads_changed.clone(),
                        self.0.configuration_changed.clone(),
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
    pub(crate) fn install(
        &self,
        configuration: &Rc<CommittedConfiguration>,
        derived: &Rc<crate::configuration::DerivedConfiguration>,
        operations: &[PlanOperation],
        prepared: PreparedSources,
    ) -> Vec<Rc<SourceEntry>> {
        self.planned();
        let mut routing_affected = operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.kind,
                    OperationKind::Added
                        | OperationKind::Removed
                        | OperationKind::ReplacedNative
                        | OperationKind::ReplacedAuthority
                )
            })
            .map(|operation| operation.key.id.clone())
            .collect::<BTreeSet<_>>();
        let rules_changed = {
            let graph = self.0.graph.borrow();
            // Both versions matter when a group loses a member or its rule.
            graph
                .rules
                .keys()
                .chain(derived.rules.keys())
                .filter(|did| graph.rules.get(*did) != derived.rules.get(*did))
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let changed = !prepared.installed.is_empty()
            || !prepared.removed.is_empty()
            || !rules_changed.is_empty();
        routing_affected.extend(rules_changed);
        for key in &prepared.removed {
            self.0.retire_work(&key.id);
            if let Some(slot) = self.0.directory.borrow_mut().devices.get_mut(&key.id) {
                slot.routing_history = None;
            }
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
                        self.0.write_gates.clear_authority(&did);
                    }
                }
                OperationKind::Removed | OperationKind::Reused => {
                    unreachable!("only new instances need prepared resources")
                }
            }
            self.0
                .directory
                .borrow_mut()
                .devices
                .get_mut(&did)
                .unwrap()
                .work = Some(source.work.clone());
            started.push(source);
        }
        // Reused entries receive committed metadata without preparing resources
        // or refreshing their unchanged native feedback.
        for operation in operations {
            if operation.kind == OperationKind::Removed {
                continue;
            }
            let device = configuration.device(&operation.key).unwrap();
            let work = self.0.directory.borrow().devices[&operation.key.id]
                .work
                .clone()
                .unwrap();
            work.physical().set_label(device.label.clone());
            work.set_committed_battery_issue(
                device
                    .native
                    .as_ref()
                    .and_then(|native| native.device().battery_issue()),
            );
        }
        // Install the configuration and graph together before starting consumers
        // or deriving group feedback. Each group is recomputed once per commit.
        self.set_configuration(configuration.clone(), derived.clone());
        for source in started {
            let did = &source.key.id;
            self.0
                .add_consumers(source.work.clone(), source.sink, source.receivers);
            if source.kind != OperationKind::ReplacedLogical {
                self.0.install_work(did);
            }
            self.0.update_device_availability(did);
            source.work.report(&self.0);
        }
        if changed {
            for did in derived.rules.keys() {
                self.0.update_device_availability(did);
            }
            self.0.refresh_all_group_states();
        }
        {
            let directory = self.0.directory.borrow();
            routing_affected.retain(|did| {
                directory
                    .devices
                    .get(did)
                    .is_some_and(|slot| slot.work.is_some())
            });
        }
        if !routing_affected.is_empty() {
            self.0.update_routing_for(
                routing_affected.iter().map(String::as_str),
                "committed configuration installed",
                None,
            );
        }
        let entries = self.entries(configuration);
        self.0.changed.notify(usize::MAX);
        entries
    }

    pub(crate) fn set_label(&self, key: &DeviceKey, label: &str) {
        if let Some(work) = self
            .0
            .directory
            .borrow()
            .devices
            .get(&key.id)
            .and_then(|slot| slot.work.as_ref())
        {
            work.physical().set_label(label.into());
        }
    }

    pub(crate) fn pending(
        &self,
        key: &DeviceKey,
        committed: &CommittedConfiguration,
        reason: String,
    ) {
        {
            let mut directory = self.0.directory.borrow_mut();
            let committed_name = (directory.scope == committed.scope)
                .then(|| committed.device(key).map(|device| device.label.clone()))
                .flatten();
            directory.planning_failed(&key.id, committed_name, reason);
        }
        self.0.status_changed.notify(usize::MAX);
    }

    pub(crate) fn planned(&self) {
        let changed = self.0.directory.borrow_mut().planned();
        if changed {
            self.0.status_changed.notify(usize::MAX);
        }
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
        let (admitted, cached) = self
            .directory
            .borrow_mut()
            .devices
            .get_mut(did)
            .map(|slot| {
                slot.routing_history = None;
                (slot.work.clone(), slot.latest_description.is_some())
            })
            .unwrap_or_default();
        if let Some(work) = &admitted {
            let sink = work.current_sink();
            sink.physical
                .set_availability(Availability::Unavailable(reason.into()));
            work.retire();
        }
        self.refresh_group_states(did);
        self.store.lan().remove(did)?;
        if admitted.is_some() || cached {
            self.store
                .devices()
                .record_revocations(&[crate::configuration::Revocation {
                    key,
                    reason: reason.into(),
                }])?;
        }
        Ok(())
    }
}
