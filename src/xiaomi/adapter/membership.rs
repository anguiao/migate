use super::{
    directory::{DescriptionOperation, DescriptionProblem},
    model::AdapterError,
    state::{Context, LogicalSink},
    work::DeviceWork,
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey},
    xiaomi::central::DeviceEntry,
};
use std::rc::Rc;
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
                                context.status_changed.notify(usize::MAX);
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
            .directory
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.observe_access_filtered(&dids, entries);
    }
    pub(super) fn observe_access_filtered(&self, dids: &[String], entries: Vec<DeviceEntry>) {
        let observed = dids
            .iter()
            .filter(|did| self.directory.borrow().devices.contains_key(*did))
            .cloned()
            .collect::<Vec<_>>();
        self.hub.borrow_mut().observe_access(&observed, entries);
        for did in &observed {
            self.update_availability(did);
        }
        self.update_routing_for(
            observed.iter().map(String::as_str),
            "hub device access changed",
            None,
        );
    }
    pub(super) fn reconcile_all(self: &Rc<Self>) -> Result<(), AdapterError> {
        let dids = self
            .directory
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
        if self.directory.borrow().group(did).is_some() {
            return self.reconcile_group(did);
        }
        if self.directory.borrow().devices[did].work.is_some() {
            self.install_work(did);
            self.update_availability(did);
            if let Some(work) = self.directory.borrow().devices[did].work.as_ref() {
                work.retry_subscriptions();
            }
        }
        self.stage();
        Ok(())
    }
    /// A failed directory description is diagnostic only: the committed
    /// runtime keeps serving and the latest pending fact is retained.
    pub(super) fn pending_description(&self, did: &str, reason: String) {
        {
            let mut directory = self.directory.borrow_mut();
            let target = directory.description_target(did);
            if let Some(slot) = directory.devices.get_mut(did) {
                slot.description_failed(DescriptionProblem {
                    operation: DescriptionOperation::Load,
                    target,
                    reason,
                });
            }
        }
        self.status_changed.notify(usize::MAX);
        self.changed.notify(usize::MAX);
    }

    pub(super) fn resolved_description_target(&self, did: &str) {
        let cleared = {
            let mut directory = self.directory.borrow_mut();
            let target = directory.description_target(did);
            let slot = directory
                .devices
                .get_mut(did)
                .expect("a retained source has a slot");
            let matches = target
                .as_ref()
                .zip(slot.latest_description.as_ref())
                .is_some_and(|((model, urn), source)| {
                    source.raw.model == *model && source.raw.instance_type == *urn
                });
            if slot.loading.is_some() || !matches {
                false
            } else {
                let before = slot.description_problems.len();
                slot.description_problems
                    .retain(|problem| problem.operation != DescriptionOperation::Load);
                slot.description_problems.len() != before
            }
        };
        if cleared {
            self.status_changed.notify(usize::MAX);
        }
    }
    /// Retire one native runtime and its current logical sink.
    pub(super) fn retire_work(&self, did: &str) {
        let work = self
            .directory
            .borrow_mut()
            .devices
            .get_mut(did)
            .and_then(|slot| slot.work.take());
        if let Some(work) = work {
            let physical = work.physical();
            physical.set_availability(Availability::Unavailable("source retired".into()));
            self.registry.remove(physical.key());
            work.retire();
        }
    }
    pub(super) fn remove_source_slot(&self, did: &str) {
        self.registry.remove(&DeviceKey::new("xiaomi", did));
        self.retire_work(did);
        let slot = self.directory.borrow_mut().remove_source(did);
        if let Some(slot) = slot
            && let Some(loading) = slot.loading
        {
            loading.life.close();
        }
        self.hub.borrow_mut().forget_device(did);
        self.update_device_routing(did, "device removed");
        self.commands.remove(did);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn remove(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        if self.directory.borrow().group(did).is_some() {
            return self.remove_group(did);
        }
        self.revoke(did, "device removed from the selected home")?;
        self.remove_source_slot(did);
        if let Some(session) = self.notifications.borrow().as_ref() {
            drop(session.handle.unsubscribe_device(did));
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }
}
