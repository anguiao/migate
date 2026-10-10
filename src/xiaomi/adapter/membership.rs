use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, Lifetime},
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey, PhysicalDevice},
    storage::{AdmissionError, CachedPhysicalDevice},
    xiaomi::central::DeviceEntry,
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
impl Context {
    pub(super) fn add_consumers(
        self: &Rc<Self>,
        record: Rc<DeviceRecord>,
        receivers: Vec<CommandReceiver>,
    ) {
        for (function, receiver) in record.physical.functions().iter().zip(receivers) {
            if function.description().category == crate::device::Category::Light {
                let function = function.clone();
                let context = self.clone();
                let life = record.clone();
                self.tasks.spawn(async move {
                    let mut changes = function.subscribe_light_actions().expect("light");
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
        if let Some(record) = admitted {
            if *record.cached.borrow() != cached {
                match self
                    .store
                    .devices()
                    .publish_cached(&cached, &record.physical.label())
                {
                    Ok(stored) => {
                        let bindings = |device: &CachedPhysicalDevice| {
                            device
                                .device
                                .functions
                                .iter()
                                .map(|function| {
                                    (
                                        function.description.id.clone(),
                                        (
                                            function.power,
                                            function.lighting.clone(),
                                            function.description.capabilities.clone(),
                                        ),
                                    )
                                })
                                .collect::<BTreeMap<_, _>>()
                        };
                        let changed = bindings(&record.cached.borrow()) != bindings(&cached)
                            || record.cached.borrow().device.refresh_trigger
                                != cached.device.refresh_trigger;
                        if changed {
                            if let Some(session) = &self.live.borrow().session {
                                drop(session.handle.unsubscribe_properties(did));
                            }
                            if let Some(session) = self.notifications.borrow().as_ref() {
                                drop(session.handle.unsubscribe_properties(did));
                            }
                            record
                                .binding_generation
                                .set(record.binding_generation.get() + 1);
                        }
                        let replace = changed
                            && cached
                                .device
                                .functions
                                .iter()
                                .any(|function| function.lighting.is_some());
                        if replace {
                            let (physical, receivers) = PhysicalDevice::new(stored.description());
                            // Publish the new instance before retiring the old
                            // runtime; a rejected replacement leaves the running
                            // record untouched and blocks the confirmed shape.
                            match self.registry.replace(physical.clone()) {
                                Ok(_) => {
                                    record.life.close();
                                    {
                                        let mut live = self.live.borrow_mut();
                                        if let Some(work) =
                                            live.devices.get_mut(did).unwrap().work.take()
                                        {
                                            work.life.close();
                                        }
                                        if let Some(session) = live.session.as_mut() {
                                            session.devices.remove(did);
                                        }
                                    }
                                    let replacement = Rc::new(DeviceRecord {
                                        life: Lifetime::default(),
                                        binding_generation: std::cell::Cell::new(
                                            record.binding_generation.get(),
                                        ),
                                        cached: RefCell::new(cached),
                                        physical,
                                    });
                                    // Observers poll only after the complete replacement; storage identity is unchanged.
                                    self.live
                                        .borrow_mut()
                                        .devices
                                        .get_mut(did)
                                        .unwrap()
                                        .admitted = Some(replacement.clone());
                                    self.add_consumers(replacement, receivers);
                                }
                                Err(error) => self.block_unsupported(did, error.to_string()),
                            }
                        } else {
                            *record.cached.borrow_mut() = cached;
                            if changed && let Some(work) = &self.live.borrow().devices[did].work {
                                work.bindings_changed();
                            }
                        }
                    }
                    Err(AdmissionError::Storage(error)) => return Err(error.into()),
                    Err(error) => {
                        if matches!(error, AdmissionError::IncompatibleFunctions) {
                            self.block_unsupported(did, error.to_string());
                            // A confirmed unsupported description revokes its
                            // group immediately, not on an unrelated event.
                            self.reconcile_groups()?;
                        } else {
                            self.block_device(did, error.to_string());
                        }
                        return Ok(());
                    }
                }
            }
        } else {
            let label = name
                .as_deref()
                .map(crate::storage::import_label)
                .unwrap_or_else(|| crate::runtime::default_device_label(&cached.model, did));
            let stored = match self.store.devices().publish_cached(&cached, &label) {
                Ok(stored) => stored,
                Err(AdmissionError::Storage(error)) => return Err(error.into()),
                Err(error) => {
                    let unsupported = matches!(error, AdmissionError::IncompatibleFunctions);
                    if unsupported {
                        self.live
                            .borrow_mut()
                            .devices
                            .get_mut(did)
                            .unwrap()
                            .blocked_unsupported = true;
                    }
                    self.problem(did, Some(error.to_string()));
                    if unsupported {
                        self.reconcile_groups()?;
                    }
                    return Ok(());
                }
            };
            let (physical, receivers) = PhysicalDevice::new(stored.description());
            self.registry.insert(physical.clone());
            let record = Rc::new(DeviceRecord {
                life: Lifetime::default(),
                binding_generation: std::cell::Cell::new(0),
                cached: RefCell::new(cached),
                physical,
            });
            {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.get_mut(did).unwrap();
                slot.admitted = Some(record.clone());
                slot.blocked = None;
                slot.blocked_unsupported = false;
            }
            self.add_consumers(record, receivers);
        }
        self.live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .candidate = None;
        self.install_work(did);
        self.update_availability(did);
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let diagnostic = slot.work.as_ref().and_then(|work| work.diagnostic());
        let problem = slot.blocked.clone().or(diagnostic);
        drop(live);
        self.problem(did, problem);
        if let Some(work) = self.live.borrow().devices[did].work.as_ref() {
            work.retry_subscriptions();
        }
        self.changed.notify(usize::MAX);
        self.reconcile_groups()?;
        Ok(())
    }
    pub(super) fn unavailable_device(&self, did: &str, reason: String) {
        if let Some(record) = &self.live.borrow().devices[did].admitted {
            record
                .physical
                .set_availability(Availability::Unavailable(reason));
        }
    }
    pub(super) fn block_device(&self, did: &str, reason: String) {
        self.block(did, reason, false);
    }
    /// Block a member whose description is present but confirmed unsupported;
    /// this distinction keeps temporary failures from revoking valid groups.
    pub(super) fn block_unsupported(&self, did: &str, reason: String) {
        self.block(did, reason, true);
    }
    fn block(&self, did: &str, reason: String, unsupported: bool) {
        {
            let mut live = self.live.borrow_mut();
            let slot = live.devices.get_mut(did).unwrap();
            slot.blocked = Some(reason.clone());
            // A previously confirmed unsupported description stays authoritative
            // until a fresh supported description replaces it; an unrelated
            // temporary failure must not silently re-admit the old record.
            slot.blocked_unsupported |= unsupported;
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
                        .any(|function| function.lighting.is_some())
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
