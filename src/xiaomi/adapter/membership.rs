use super::{
    model::{AdapterError, INCOMPATIBLE_FUNCTIONS},
    state::{Context, DeviceRecord},
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey},
    xiaomi::central::DeviceEntry,
};
use std::{collections::BTreeMap, rc::Rc};
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
        if self.live.borrow().devices[did].group.is_some() {
            return self.reconcile_group(did);
        }
        let incompatible = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            slot.candidate
                .as_ref()
                .zip(slot.admitted.as_ref())
                .is_some_and(|((cached, _), record)| {
                    !super::publication::same_shape(
                        &record.physical.description(),
                        &cached.physical_description(record.physical.label()),
                    )
                })
        };
        if incompatible {
            {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.get_mut(did).unwrap();
                slot.candidate = None;
            }
            self.block_device(did, INCOMPATIBLE_FUNCTIONS.to_owned());
            return Ok(());
        }
        if self.live.borrow().devices[did].candidate.is_some() {
            self.problem(did, Some("configuration pending".into()));
        }
        if self.live.borrow().devices[did].admitted.is_some() {
            self.install_work(did);
            self.update_availability(did);
            if let Some(work) = self.live.borrow().devices[did].work.as_ref() {
                work.retry_subscriptions();
            }
        }
        self.stage();
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
        self.refresh_repair_groups(did);
        self.problem(did, Some(reason));
    }
    pub(super) fn remove(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        if self
            .live
            .borrow()
            .devices
            .get(did)
            .is_some_and(|slot| slot.group.is_some())
        {
            return self.remove_group(did);
        }
        let key = DeviceKey::new("xiaomi", did);
        self.revoke(did, "device removed from the selected home")?;
        self.registry.remove(&key);
        let handle = {
            let mut live = self.live.borrow_mut();
            let slot = live.devices.remove(did).unwrap();
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
                if let Some(work) = session.devices.remove(did) {
                    work.life.close();
                }
                Some((session.handle.clone(), light))
            } else {
                None
            }
        };
        self.problem(did, None);
        self.commands.remove(did);
        if let Some(session) = self.notifications.borrow().as_ref() {
            drop(session.handle.unsubscribe_device(did));
        }
        if let Some((handle, light)) = handle {
            if light {
                // Retire delivery and unsent guards while the original sent receipt keeps its gate.
                drop(handle.unsubscribe_properties(did));
            } else {
                drop(handle.unsubscribe_device(did));
            }
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }
}
