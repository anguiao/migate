//! The confirmed source inputs and the serving device instances have one
//! synchronous handoff after the complete configuration transaction commits.

use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, GroupSource, Lifetime},
};
use crate::{
    device::{
        Availability, CommandReceiver, DeviceKey, DeviceRegistry, PhysicalDevice,
        PhysicalDeviceDescription,
    },
    storage::{CachedPhysicalDevice, CachedScope, PublicationState, StorageError, StoredRelation},
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
        let mut live = self.0.live.borrow_mut();
        let mut entries = Vec::new();
        for slot in live.devices.values_mut() {
            let (cached, name) = match &slot.candidate {
                Some((cached, name)) => (cached.clone(), name.clone()),
                None => match &slot.admitted {
                    Some(record) => (record.cached.borrow().clone(), None),
                    None => continue,
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
                                &cached.device.key.id,
                            )
                        })
                });
            let description = cached.physical_description(label);
            if let Some(prepared) = &slot.prepared
                && prepared.cached == cached
                && prepared.group == slot.group
            {
                entries.push(prepared.clone());
                continue;
            }
            let existing = slot.admitted.as_ref().filter(|record| {
                !record.life.is_closed() && same_shape(&record.physical.description(), &description)
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
            let prepared = Rc::new(SourceEntry {
                cached,
                physical: record.physical.clone(),
                fresh: slot.confirmed,
                problem: slot.blocked.clone(),
                record,
                receivers: RefCell::new(receivers),
                group: slot.group.clone(),
            });
            slot.prepared = Some(prepared.clone());
            entries.push(prepared);
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
        for entry in &snapshot.entries {
            let did = &entry.cached.device.key.id;
            let old = self.0.live.borrow().devices[did].admitted.clone();
            let replaced = old
                .as_ref()
                .is_some_and(|old| !Rc::ptr_eq(old, &entry.record));
            let bindings_changed = old
                .as_ref()
                .is_some_and(|old| !same_bindings(&old.cached.borrow(), &entry.cached));
            let group_changed = self.0.live.borrow().devices[did].effective_group != entry.group;
            if replaced || bindings_changed {
                self.0.retire_device_notifications(did);
                let mut live = self.0.live.borrow_mut();
                if let Some(work) = live.devices.get_mut(did).unwrap().work.take() {
                    work.life.close();
                }
                if let Some(session) = live.session.as_mut()
                    && let Some(work) = session.devices.remove(did)
                {
                    work.life.close();
                }
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
                slot.candidate = None;
                slot.prepared = None;
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
            if (old.is_none() || replaced || bindings_changed || group_changed)
                && let Some(work) = self.0.live.borrow().devices[did].work.as_ref()
            {
                work.request_member_read();
            }
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

pub(super) fn same_shape(a: &PhysicalDeviceDescription, b: &PhysicalDeviceDescription) -> bool {
    a.key == b.key
        && a.functions.len() == b.functions.len()
        && a.functions.iter().all(|old| {
            b.functions.iter().any(|new| {
                old.id == new.id
                    && old.category == new.category
                    && old.light_profile() == new.light_profile()
                    && old.temperature_range().map(|range| range.mired_bounds())
                        == new.temperature_range().map(|range| range.mired_bounds())
            })
        })
}

fn same_bindings(a: &CachedPhysicalDevice, b: &CachedPhysicalDevice) -> bool {
    let bindings = |cached: &CachedPhysicalDevice| {
        cached
            .device
            .functions
            .iter()
            .map(|function| {
                (
                    function.description.id.clone(),
                    (function.power, function.lighting.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    a.device.refresh_trigger == b.device.refresh_trigger && bindings(a) == bindings(b)
}

impl Context {
    pub(super) fn stage(&self) {
        self.configuration_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }

    /// Confirmed removal closes every retained authority before persistence.
    pub(super) fn revoke(&self, did: &str, reason: &str) -> Result<(), AdapterError> {
        let key = DeviceKey::new("xiaomi", did);
        let had_authority = self
            .live
            .borrow()
            .devices
            .get(did)
            .is_some_and(|slot| slot.admitted.is_some());
        if let Some(slot) = self.live.borrow().devices.get(did) {
            for physical in slot
                .admitted
                .iter()
                .map(|record| &record.physical)
                .chain(slot.prepared.iter().map(|entry| &entry.physical))
            {
                physical.set_availability(Availability::Unavailable(reason.into()));
                physical.close();
            }
        }
        if had_authority {
            self.store
                .devices()
                .record_revocations(&[crate::storage::StoredRevocation {
                    key,
                    reason: reason.into(),
                }])?;
        }
        Ok(())
    }
}
