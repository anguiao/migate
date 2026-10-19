use super::{
    model::AdapterError,
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey},
    xiaomi::central::DeviceEntry,
};
use std::{collections::BTreeMap, rc::Rc};
impl Context {
    pub(super) fn add_consumers(
        self: &Rc<Self>,
        work: Rc<DeviceWork>,
        sink: Rc<LogicalSink>,
        receivers: Vec<CommandReceiver>,
    ) {
        for (function, receiver) in sink.physical.functions().iter().zip(receivers) {
            if function.description().kind.is_light() {
                let function = function.clone();
                let context = self.clone();
                let life = sink.clone();
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
                work.clone(),
                sink.clone(),
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
        if self.live.borrow().devices[did].candidate.is_some() {
            self.problem(did, Some("configuration pending".into()));
        }
        if self.live.borrow().devices[did].admitted.is_some() {
            self.install_work(did);
            self.update_availability(did);
            if let Some(work) = self.live.borrow().devices[did].admitted.as_ref() {
                work.retry_subscriptions();
            }
        }
        self.stage();
        Ok(())
    }
    pub(super) fn unavailable_device(&self, did: &str, reason: String) {
        if let Some(work) = &self.live.borrow().devices[did].admitted {
            work.physical()
                .set_availability(Availability::Unavailable(reason));
        }
    }
    pub(super) fn retire_work(&self, did: &str) {
        let mut live = self.live.borrow_mut();
        if let Some(work) = live
            .devices
            .get_mut(did)
            .and_then(|slot| slot.admitted.take())
        {
            let sink = work.current_sink();
            sink.physical
                .set_availability(Availability::Unavailable("source retired".into()));
            self.registry.remove(sink.physical.key());
            work.life.close();
            sink.close();
        }
        if let Some(work) = live
            .session
            .as_mut()
            .and_then(|session| session.devices.remove(did))
        {
            let sink = work.current_sink();
            sink.physical
                .set_availability(Availability::Unavailable("source retired".into()));
            work.life.close();
            sink.close();
        }
    }
    pub(super) fn remove_source_slot(&self, did: &str) {
        self.registry.remove(&DeviceKey::new("xiaomi", did));
        self.retire_work(did);
        if let Some(slot) = self.live.borrow_mut().devices.remove(did) {
            if let Some(life) = slot.loading {
                life.close();
            }
            if let Some(work) = slot.admitted {
                work.life.close();
                work.current_sink().close();
            }
        }
        self.problem(did, None);
        self.commands.remove(did);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn block_device(&self, did: &str, reason: String) {
        {
            let mut live = self.live.borrow_mut();
            let slot = live.devices.get_mut(did).unwrap();
            slot.blocked = Some(reason.clone());
        }
        self.retire_work(did);
        if let Some(session) = self.live.borrow().session.as_ref() {
            drop(session.handle.unsubscribe_properties(did));
        }
        if let Some(session) = self.notifications.borrow().as_ref() {
            drop(session.handle.unsubscribe_properties(did));
        }
        self.changed.notify(usize::MAX);
        self.unavailable_device(did, reason.clone());
        self.refresh_group_states(did);
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
        self.revoke(did, "device removed from the selected home")?;
        let handle = {
            let live = self.live.borrow();
            let light = live.devices[did].admitted.as_ref().is_some_and(|work| {
                work.native
                    .device
                    .functions()
                    .iter()
                    .any(|function| function.lighting().is_some())
            });
            live.session
                .as_ref()
                .map(|session| (session.handle.clone(), light))
        };
        self.remove_source_slot(did);
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
