use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, Lifetime},
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey, PhysicalDevice},
    storage::{AdmissionError, CachedPhysicalDevice},
    xiaomi::{capability::ParsedDevice, central::DeviceEntry},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
impl Context {
    pub(super) fn add_consumers(
        self: &Rc<Self>,
        record: Rc<DeviceRecord>,
        receivers: Vec<CommandReceiver>,
    ) {
        for (function, receiver) in record.physical.functions().iter().zip(receivers) {
            if function.description().category == crate::device::Category::Light
                && let Some(mut changes) = function.subscribe_light_actions()
            {
                let context = self.clone();
                let life = record.clone();
                self.tasks.spawn(async move {
                    futures_lite::future::or(
                        async {
                            life.life.ended().await;
                            Ok(())
                        },
                        async {
                            loop {
                                changes.changed().await;
                                context.update_routing("lighting action changed");
                            }
                        },
                    )
                    .await
                });
            }
            self.tasks.spawn(super::commands::consume(
                receiver,
                self.clone(),
                record.clone(),
                function.function_key().id.clone(),
            ));
        }
    }
    /// Apply only observations in wire order; admission and storage run after the request future wakes.
    pub(super) fn observe_access(&self, entries: Vec<DeviceEntry>) {
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.observe_access_filtered(&dids, entries);
    }
    pub(super) fn observe_access_filtered(&self, dids: &[String], entries: Vec<DeviceEntry>) {
        let mut entries = entries
            .into_iter()
            .map(|entry| (entry.did, entry.access))
            .collect::<BTreeMap<_, _>>();
        let observed = {
            let mut live = self.live.borrow_mut();
            let mut observed = Vec::new();
            for did in dids {
                if let Some(slot) = live.devices.get_mut(did) {
                    slot.access = entries.remove(did);
                    observed.push(did);
                }
            }
            observed
        };
        for did in observed {
            self.update_availability(did);
        }
    }
    pub(super) fn reconcile_all(self: &Rc<Self>) -> Result<(), AdapterError> {
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.reconcile(&did)?;
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }
    pub(super) fn reconcile(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        let (candidate, admitted) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (slot.candidate.clone(), slot.admitted.clone())
        };
        let Some((cached, name)) = candidate.or_else(|| {
            admitted
                .as_ref()
                .map(|record| (record.cached.borrow().clone(), None))
        }) else {
            let reason = self.live.borrow().devices[did]
                .blocked
                .clone()
                .unwrap_or_else(|| "waiting for a complete device description".into());
            self.problem(did, Some(reason));
            return Ok(());
        };
        let proceed = match &admitted {
            Some(record) if !same_cache_semantics(&record.cached.borrow(), &cached) => {
                self.admit_update(did, record, &cached)?
            }
            Some(_) => true,
            None => {
                let label = name
                    .as_deref()
                    .map(crate::storage::import_label)
                    .unwrap_or_else(|| crate::runtime::default_device_label(&cached.model, did));
                self.admit_new(did, &cached, &label)?
            }
        };
        if !proceed {
            return Ok(());
        }
        self.live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .candidate = None;
        if self.live.borrow().devices[did].unpublished.is_none() {
            self.install_work(did);
        }
        self.update_availability(did);
        let (problem, diagnostic) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (
                slot.blocked.clone().or_else(|| slot.unpublished.clone()),
                slot.work.as_ref().and_then(|work| work.diagnostic()),
            )
        };
        self.problem(did, problem.or(diagnostic));
        if let Some(work) = self.live.borrow().devices[did].work.as_ref() {
            work.retry_subscriptions();
        }
        self.changed.notify(usize::MAX);
        self.reconcile_groups()?;
        Ok(())
    }

    fn admit_update(
        self: &Rc<Self>,
        did: &str,
        record: &Rc<DeviceRecord>,
        cached: &CachedPhysicalDevice,
    ) -> Result<bool, AdapterError> {
        match self
            .store
            .devices()
            .publish_cached(cached, &record.physical.label())
        {
            Ok(_) => {
                let changed = !same_device_behavior(&record.cached.borrow().device, &cached.device);
                if changed {
                    // Invalidate old operations without replacing the physical
                    // instance, published capabilities or command consumers.
                    record
                        .binding_generation
                        .set(record.binding_generation.get() + 1);
                    if let Some(session) = self.live.borrow().session.as_ref() {
                        drop(session.handle.unsubscribe_properties(did));
                    }
                    if let Some(session) = self.notifications.borrow().as_ref() {
                        drop(session.handle.unsubscribe_properties(did));
                    }
                    self.unavailable_device(did, "device bindings changed".into());
                }
                *record.cached.borrow_mut() = cached.clone();
                if changed && let Some(work) = &self.live.borrow().devices[did].work {
                    work.bindings_changed();
                }
                self.live.borrow_mut().devices.get_mut(did).unwrap().blocked = None;
                Ok(true)
            }
            Err(AdmissionError::Storage(error)) => Err(error.into()),
            Err(error) => {
                self.block_device(did, error.to_string());
                Ok(false)
            }
        }
    }

    fn admit_new(
        self: &Rc<Self>,
        did: &str,
        cached: &CachedPhysicalDevice,
        label: &str,
    ) -> Result<bool, AdapterError> {
        let stored = match self.store.devices().publish_cached(cached, label) {
            Ok(stored) => stored,
            Err(AdmissionError::Storage(error)) => return Err(error.into()),
            Err(error) => {
                self.problem(did, Some(error.to_string()));
                return Ok(false);
            }
        };
        let problem = publication_problem(&cached.device);
        let (physical, receivers) = PhysicalDevice::new(stored.description());
        let record = Rc::new(DeviceRecord {
            life: Lifetime::default(),
            binding_generation: std::cell::Cell::new(0),
            cached: RefCell::new(cached.clone()),
            physical: physical.clone(),
        });
        {
            let mut live = self.live.borrow_mut();
            let slot = live.devices.get_mut(did).unwrap();
            slot.admitted = Some(record.clone());
            slot.blocked = None;
            slot.unpublished = problem.clone();
        }
        if let Some(reason) = problem {
            physical.set_availability(Availability::Unavailable(reason));
            drop(receivers);
        } else {
            self.registry.insert(physical);
            self.add_consumers(record, receivers);
        }
        Ok(true)
    }

    pub(super) fn unavailable_device(&self, did: &str, reason: String) {
        if let Some(record) = &self.live.borrow().devices[did].admitted {
            record
                .physical
                .set_availability(Availability::Unavailable(reason));
        }
    }
    pub(super) fn block_device(&self, did: &str, reason: String) {
        {
            let mut live = self.live.borrow_mut();
            let slot = live.devices.get_mut(did).unwrap();
            slot.blocked = Some(reason.clone());
            if let Some(record) = &slot.admitted {
                record
                    .binding_generation
                    .set(record.binding_generation.get() + 1);
            }
            if let Some(work) = slot.work.take() {
                work.life.close();
            }
            if let Some(session) = live.session.as_mut() {
                session.devices.remove(did);
                drop(session.handle.unsubscribe_properties(did));
            }
        }
        if let Some(session) = self.notifications.borrow().as_ref() {
            drop(session.handle.unsubscribe_properties(did));
        }
        self.changed.notify(usize::MAX);
        self.unavailable_device(did, reason.clone());
        self.problem(did, Some(reason));
    }
    #[cfg(test)]
    pub(super) fn remove(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        self.reconcile_groups_removed(&[DeviceKey::new("xiaomi", did)])
    }
    /// Retire the runtime resources of devices whose deletion was already
    /// persisted and published atomically. Registry physical devices were
    /// closed by the versioned composition update.
    pub(super) fn retire_confirmed_removed(&self, removed: &[DeviceKey]) {
        for key in removed {
            if key.source != "xiaomi" {
                continue;
            }
            let did = key.id.clone();
            let handle = {
                let mut live = self.live.borrow_mut();
                let Some(slot) = live.devices.remove(&did) else {
                    continue;
                };
                let light = slot.admitted.as_ref().is_some_and(|record| {
                    record
                        .cached
                        .borrow()
                        .device
                        .functions
                        .iter()
                        .any(|function| {
                            function.description.category == crate::device::Category::Light
                        })
                });
                if let Some(life) = slot.loading {
                    life.close();
                }
                if let Some(record) = slot.admitted {
                    record.life.close();
                }
                if let Some(work) = slot.work {
                    work.life.close();
                }
                if let Some(session) = live.session.as_mut() {
                    if let Some(work) = session.devices.remove(&did) {
                        work.life.close();
                    }
                    Some((session.handle.clone(), light))
                } else {
                    None
                }
            };
            self.problem(&did, None);
            self.commands.remove(&did);
            if let Some(session) = self.notifications.borrow().as_ref() {
                drop(session.handle.unsubscribe_device(&did));
            }
            if let Some((handle, light)) = handle {
                if light {
                    // Retire delivery and unsent guards while the original sent receipt keeps its gate.
                    drop(handle.unsubscribe_properties(&did));
                } else {
                    drop(handle.unsubscribe_device(&did));
                }
            }
        }
    }
}

/// The Matter publication problem of one admitted Xiaomi device description.
/// Outlets are always publishable; a light must pass the shared mapping.
pub(super) fn publication_problem(device: &ParsedDevice) -> Option<String> {
    device.functions.iter().find_map(|function| {
        if function.description.category != crate::device::Category::Light {
            return None;
        }
        crate::matter::publication::map_light(&function.description)
            .err()
            .map(|reason| reason.as_str().to_string())
    })
}

/// Cache equality that treats an equivalent capability reordering as no
/// change, so a redundant description never restarts a device.
fn same_cache_semantics(left: &CachedPhysicalDevice, right: &CachedPhysicalDevice) -> bool {
    left.uid == right.uid
        && left.home_id == right.home_id
        && left.group_id == right.group_id
        && left.model == right.model
        && left.instance_type == right.instance_type
        && left.device.key == right.device.key
        && same_device_behavior(&left.device, &right.device)
}

/// Behavior equality ignores metadata that carries no control meaning but
/// keeps every binding, range and property that commands depend on.
fn same_device_behavior(left: &ParsedDevice, right: &ParsedDevice) -> bool {
    left.key == right.key
        && left.refresh_trigger == right.refresh_trigger
        && left.functions.len() == right.functions.len()
        && left.functions.iter().all(|function| {
            right.functions.iter().any(|candidate| {
                candidate.description.id == function.description.id
                    && candidate.description.category == function.description.category
                    && candidate.power == function.power
                    && candidate.lighting == function.lighting
                    && candidate
                        .description
                        .same_capabilities(&function.description)
            })
        })
}
