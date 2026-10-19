//! The confirmed source inputs and the serving device instances have one
//! synchronous handoff after the complete configuration transaction commits.

use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, GroupSource, Lifetime},
};
use crate::{
    device::{
        Availability, CommandReceiver, DeviceKey, DeviceRegistry, FunctionDescription,
        FunctionKind, PhysicalDevice, PhysicalDeviceDescription,
    },
    storage::{CachedPhysicalDevice, CachedScope, PublicationState, StorageError, StoredRelation},
    xiaomi::capability::{
        RepairPlan, effective_temperature_range, narrowed_profile, plan_rgb_repair,
        repaired_profile,
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

/// The live Xiaomi source continues running while Matter instances restart.
#[derive(Clone)]
pub struct SourceHandle(pub(super) Rc<Context>);

pub(crate) struct SourceEntry {
    pub(crate) cached: CachedPhysicalDevice,
    pub(crate) physical: PhysicalDevice,
    pub(crate) fresh: bool,
    pub(crate) problem: Option<String>,
    /// Committed logical repair inputs; `None` means the native profile serves.
    pub(crate) repair: Option<Rc<RepairPlan>>,
    /// Reason the native group stays unrepaired, retained for diagnostics only.
    pub(crate) repair_reason: Option<String>,
    record: Rc<DeviceRecord>,
    receivers: RefCell<Option<Vec<CommandReceiver>>>,
    group: Option<GroupSource>,
}

#[derive(Clone)]
pub(crate) struct SourceSnapshot {
    pub(crate) scope: Option<CachedScope>,
    pub(crate) entries: Vec<Rc<SourceEntry>>,
    pub(crate) relations: Vec<StoredRelation>,
}

impl SourceHandle {
    pub fn registry(&self) -> DeviceRegistry {
        self.0.registry.clone()
    }

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

    pub(crate) fn committed(&self) -> Result<(SourceSnapshot, PublicationState), StorageError> {
        let uid = self
            .0
            .live
            .borrow()
            .credentials
            .as_ref()
            .map(|record| record.uid.clone());
        let scope = uid
            .map(|uid| self.0.store.devices().load_scope(&uid))
            .transpose()?
            .flatten();
        let saved = self.0.store.devices().load_publication(scope.as_ref())?;
        let mut entries = self
            .0
            .live
            .borrow()
            .devices
            .values()
            .filter_map(|slot| {
                let record = slot.admitted.as_ref()?;
                Some(Rc::new(SourceEntry {
                    cached: record.cached.borrow().clone(),
                    physical: record.physical.clone(),
                    fresh: false,
                    problem: slot.blocked.clone(),
                    repair: slot.effective_repair.clone(),
                    repair_reason: None,
                    record: record.clone(),
                    receivers: RefCell::new(None),
                    group: slot.effective_group.clone(),
                }))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.physical.key().cmp(b.physical.key()));
        Ok((
            SourceSnapshot {
                scope,
                entries,
                relations: saved.relations.clone(),
            },
            saved,
        ))
    }

    /// Prepare resources without changing any serving reference or binding.
    /// The caller commits and installs synchronously, with no intervening await.
    pub(crate) fn snapshot(&self) -> SourceSnapshot {
        #[cfg(test)]
        if let Some(observer) = self.0.snapshot_observer.borrow_mut().as_mut() {
            observer();
        }
        let live = self.0.live.borrow();
        let descriptions = live
            .devices
            .iter()
            .filter_map(|(did, slot)| {
                let cached = slot
                    .native_confirmed
                    .clone()
                    .or_else(|| slot.candidate.as_ref().map(|(cached, _)| cached.clone()))
                    .or_else(|| {
                        slot.admitted
                            .as_ref()
                            .map(|record| record.cached.borrow().clone())
                    })?;
                Some((did.clone(), cached))
            })
            .collect::<BTreeMap<_, _>>();
        let mut entries = Vec::new();
        for slot in live.devices.values() {
            // The latest genuine confirmed source input is authoritative for
            // planning and persistence, even when the static independent-lamp
            // contract keeps an older served logical description. A pending
            // candidate still contributes its catalog name.
            let (cached, name) = match &slot.candidate {
                Some((candidate, name)) => (
                    slot.native_confirmed
                        .clone()
                        .unwrap_or_else(|| candidate.clone()),
                    name.clone(),
                ),
                None => match &slot.native_confirmed {
                    Some(native) => (native.clone(), None),
                    None => match &slot.admitted {
                        Some(record) => (record.cached.borrow().clone(), None),
                        None => continue,
                    },
                },
            };
            let label = slot
                .admitted
                .as_ref()
                .map(|record| record.physical.label())
                .unwrap_or_else(|| {
                    name.as_deref()
                        .map(crate::storage::import_label)
                        .unwrap_or_else(|| {
                            crate::runtime::default_device_label(
                                &cached.model,
                                &cached.device.key().id,
                            )
                        })
                });
            let description = cached.physical_description(label);
            let native_interval = cached
                .device
                .functions()
                .first()
                .and_then(|function| function.lighting())
                .and_then(|light| light.temperature())
                .map(|temperature| temperature.interval());
            let (repair, repair_reason, description) = match (&slot.group, native_interval) {
                (None, _) => (None, None, description),
                (Some(_), None) => (
                    None,
                    Some("native group has no color temperature".to_string()),
                    description,
                ),
                (Some(group), Some(native)) => {
                    let known = group
                        .members
                        .iter()
                        .map(|did| descriptions.get(did))
                        .collect::<Option<Vec<_>>>();
                    match known {
                        Some(members) => {
                            let logical = members
                                .iter()
                                .map(|cached| {
                                    cached
                                        .device
                                        .functions()
                                        .first()
                                        .and_then(|function| function.lighting())
                                        .and_then(|light| light.temperature())
                                        .map(|temperature| temperature.interval())
                                })
                                .collect::<Option<Vec<_>>>()
                                .map(|intervals| effective_temperature_range(native, &intervals))
                                .unwrap_or(native);
                            let function = &cached.device.functions()[0];
                            let native_profile = function
                                .kind()
                                .light_profile()
                                .expect("a native group is a light");
                            let (repair, reason, profile) = match plan_rgb_repair(
                                &cached.device,
                                members.iter().map(|cached| {
                                    (
                                        cached.device.key().id.as_str(),
                                        cached.model.as_str(),
                                        cached.instance_type.as_str(),
                                        &cached.device,
                                    )
                                }),
                            ) {
                                Ok(Some(plan)) => {
                                    (Some(Rc::new(plan)), None, repaired_profile(logical))
                                }
                                Ok(None) => (None, None, narrowed_profile(native_profile, logical)),
                                Err(reason) => (
                                    None,
                                    Some(reason),
                                    narrowed_profile(native_profile, logical),
                                ),
                            };
                            (
                                repair,
                                reason,
                                with_light_kind(description, FunctionKind::Light(profile)),
                            )
                        }
                        None => (
                            None,
                            Some(
                                "every effective member needs a description before RGB repair"
                                    .to_string(),
                            ),
                            description,
                        ),
                    }
                }
            };
            let existing = slot.admitted.as_ref().filter(|record| {
                !record.life.is_closed() && same_shape(&record.physical.description(), &description)
            });
            // A confirmed native input rejected by the static independent-lamp
            // contract keeps its committed logical endpoint; the genuine input
            // is persisted as the source description instead.
            let existing = existing.or_else(|| {
                slot.blocked.as_ref().and_then(|_| {
                    slot.admitted
                        .as_ref()
                        .filter(|record| !record.life.is_closed())
                })
            });
            let (record, receivers) = match existing {
                Some(record) => (record.clone(), None),
                None => {
                    let (physical, receivers) = PhysicalDevice::new(description);
                    (
                        Rc::new(DeviceRecord {
                            life: Lifetime::default(),
                            binding_generation: Cell::new(0),
                            cached: RefCell::new(cached.clone()),
                            physical,
                        }),
                        Some(receivers),
                    )
                }
            };
            entries.push(Rc::new(SourceEntry {
                cached,
                physical: record.physical.clone(),
                fresh: slot.confirmed,
                problem: slot.blocked.clone(),
                repair,
                repair_reason,
                record,
                receivers: RefCell::new(receivers),
                group: slot.group.clone(),
            }));
        }
        entries.sort_by(|a, b| a.physical.key().cmp(b.physical.key()));
        let problems = live
            .devices
            .iter()
            .filter_map(|(did, slot)| slot.blocked.clone().map(|problem| (did.clone(), problem)))
            .collect::<BTreeMap<_, _>>();
        let scope = live.scope.clone();
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
                StoredRelation {
                    group_did: group.membership.group_did,
                    record_did: group.record_did,
                    model: group.model,
                    instance_type: group.instance_type,
                    is_physical: group.membership.is_physical,
                    members: group.membership.members,
                    active: group.active,
                    error,
                }
            })
            .collect();
        SourceSnapshot {
            scope,
            entries,
            relations,
        }
    }

    pub(crate) fn install(&self, snapshot: &SourceSnapshot) {
        let mut refresh = Vec::new();
        for entry in &snapshot.entries {
            let did = &entry.cached.device.key().id;
            let old = self.0.live.borrow().devices[did].admitted.clone();
            let replaced = old
                .as_ref()
                .is_some_and(|old| !Rc::ptr_eq(old, &entry.record));
            let bindings_changed = old
                .as_ref()
                .is_some_and(|old| !same_bindings(&old.cached.borrow(), &entry.cached));
            let group_changed = self.0.live.borrow().devices[did].effective_group != entry.group;
            if replaced || bindings_changed {
                self.0.light_writes.clear_authority(did);
                self.0.retire_device_notifications(did);
                self.0.retire_work(did);
            }
            if replaced {
                let old = old.as_ref().unwrap();
                old.physical
                    .set_availability(Availability::Unavailable("source replaced".into()));
                old.life.close();
                self.0.registry.remove(old.physical.key());
            }
            if bindings_changed {
                entry.physical.reset_actions();
                entry.physical.clear_light_state();
                entry
                    .record
                    .binding_generation
                    .set(entry.record.binding_generation.get() + 1);
            }
            *entry.record.cached.borrow_mut() = entry.cached.clone();
            {
                let mut live = self.0.live.borrow_mut();
                let slot = live.devices.get_mut(did).unwrap();
                slot.admitted = Some(entry.record.clone());
                slot.effective_group = entry.group.clone();
                slot.effective_repair = entry.repair.clone();
                slot.candidate = None;
            }
            self.0.registry.insert(entry.physical.clone());
            if let Some(receivers) = entry.receivers.borrow_mut().take() {
                self.0.add_consumers(entry.record.clone(), receivers);
            }
            self.0.install_work(did);
            self.0.update_availability(did);
            if let Some(work) = self.0.live.borrow().devices[did].work.as_ref() {
                work.report(&self.0, did);
            }
            if old.is_none() || replaced || bindings_changed || group_changed {
                refresh.push(did);
            }
        }
        // All members and effective relations must be installed before refresh
        // and aggregation, regardless of catalog or DID ordering.
        for did in refresh {
            self.0.refresh_source(did);
        }
        for entry in &snapshot.entries {
            self.0.refresh_group_states(&entry.cached.device.key().id);
        }
        self.0.changed.notify(usize::MAX);
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
}

fn with_light_kind(
    description: PhysicalDeviceDescription,
    kind: FunctionKind,
) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        description.key().clone(),
        description.label(),
        description
            .functions()
            .iter()
            .map(|function| FunctionDescription {
                id: function.id.clone(),
                label: function.label.clone(),
                kind: if function.kind.is_light() {
                    kind
                } else {
                    function.kind
                },
            })
            .collect(),
    )
    .expect("a light replacement keeps the description valid")
}

/// True when both descriptions publish the same fixed light surface: the
/// profile kind and the mired span, so equivalent kelvin encodings compare
/// equal regardless of the native quantization grid.
fn same_kind(a: FunctionKind, b: FunctionKind) -> bool {
    match (a.light_profile(), b.light_profile()) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.kind() == b.kind()
                && a.temperature_interval()
                    .map(crate::lighting::temperature::mired_bounds)
                    == b.temperature_interval()
                        .map(crate::lighting::temperature::mired_bounds)
        }
        _ => false,
    }
}

pub(super) fn same_shape(a: &PhysicalDeviceDescription, b: &PhysicalDeviceDescription) -> bool {
    a.key() == b.key()
        && a.functions().len() == b.functions().len()
        && a.functions().iter().all(|old| {
            b.functions()
                .iter()
                .any(|new| old.id == new.id && same_kind(old.kind, new.kind))
        })
}

fn same_bindings(a: &CachedPhysicalDevice, b: &CachedPhysicalDevice) -> bool {
    let bindings = |cached: &CachedPhysicalDevice| {
        cached
            .device
            .functions()
            .iter()
            .map(|function| {
                (
                    function.id().to_owned(),
                    (function.power(), function.lighting().cloned()),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    a.device.refresh_trigger() == b.device.refresh_trigger() && bindings(a) == bindings(b)
}

impl Context {
    pub(super) fn stage(&self) {
        self.configuration_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }

    /// The committed repair plan for one group; never a prepared candidate.
    pub(super) fn group_repair(&self, did: &str) -> Option<Rc<RepairPlan>> {
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
                .record_revocations(&[crate::storage::StoredRevocation {
                    key,
                    reason: reason.into(),
                }])?;
        }
        self.refresh_group_states(did);
        Ok(())
    }
}
