use super::*;

impl Context {
    pub(in crate::xiaomi::adapter) fn active_notification(
        &self,
        did: &str,
    ) -> Option<super::super::model::PathKind> {
        if self.is_virtual_group(did) {
            self.control_path(did)?;
            let graph = self.graph.borrow().clone();
            let group = graph.rules.get(did)?;
            if group.members.len() != group.confirmed_members {
                return None;
            }
            return group.members.iter().try_fold(
                super::super::model::PathKind::Local,
                |path, member| {
                    let member_path = if self.is_virtual_group(&member.device.id) {
                        self.active_notification(&member.device.id)?
                    } else {
                        let work = self
                            .live
                            .borrow()
                            .devices
                            .get(&member.device.id)?
                            .admitted
                            .clone()?;
                        let source = work.function_observer(&member.id)?;
                        if !self.source_valid_for(&member.device.id, &member.id, &source) {
                            return None;
                        }
                        match source {
                            NotificationSource::Local(_) => super::super::model::PathKind::Local,
                            NotificationSource::Cloud(_) => super::super::model::PathKind::Cloud,
                        }
                    };
                    Some(if member_path == super::super::model::PathKind::Cloud {
                        member_path
                    } else {
                        path
                    })
                },
            );
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone())?;
        let source = work
            .native
            .device()
            .functions()
            .iter()
            .find_map(|function| {
                work.function_observer(function.id())
                    .filter(|source| self.source_valid_for(did, function.id(), source))
            })?;
        Some(match source {
            NotificationSource::Local(_) => super::super::model::PathKind::Local,
            NotificationSource::Cloud(_) => super::super::model::PathKind::Cloud,
        })
    }
    pub(in crate::xiaomi::adapter) fn current_notifications(
        &self,
        handle: &NotificationHandle,
    ) -> bool {
        self.notifications.borrow().as_ref().is_some_and(|session| {
            session.handle.same_connection(handle)
                && connection_key(self).is_some_and(|key| key == session.key)
        })
    }
    pub(in crate::xiaomi::adapter) fn notification_source(
        &self,
        did: &str,
        id: &str,
    ) -> Option<NotificationSource> {
        // Virtual groups derive their state from ordinary member subscriptions.
        if self.is_virtual_group(did) {
            return None;
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        let local = self
            .local_candidate(did, true)
            .ok()
            .filter(|handle| {
                work.as_ref()
                    .is_some_and(|work| work.local_attached(handle))
            })
            .map(NotificationSource::Local);
        let cloud = if self.function_cloud_candidate(did, id, false).is_ok() {
            self.notifications
                .borrow()
                .as_ref()
                .filter(|session| self.current_notifications(&session.handle))
                .and_then(|session| {
                    work.as_ref()
                        .filter(|work| work.cloud_attached(&session.handle))
                        .map(|_| NotificationSource::Cloud(session.handle.clone()))
                })
        } else {
            None
        };
        let local_failed = work.as_ref().is_some_and(|work| {
            work.local_exhausted()
                && !local
                    .as_ref()
                    .is_some_and(|source| work.source_ready(id, source))
        });
        let cloud_failed = work.as_ref().is_some_and(|work| {
            work.cloud_exhausted()
                && !cloud
                    .as_ref()
                    .is_some_and(|source| work.source_ready(id, source))
        });
        let local = local.filter(|_| !local_failed);
        let cloud = cloud.filter(|_| !cloud_failed);
        match self.priority() {
            PathPriority::LocalFirst => local.or(cloud),
            PathPriority::CloudFirst => cloud.or(local),
        }
    }
    pub(in crate::xiaomi::adapter) fn source_valid_for(
        &self,
        did: &str,
        id: &str,
        source: &NotificationSource,
    ) -> bool {
        if self.is_virtual_group(did) {
            return false;
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        match source {
            NotificationSource::Local(handle) => {
                self.local_candidate(did, true)
                    .is_ok_and(|candidate| candidate.same_connection(handle))
                    && work
                        .as_ref()
                        .is_some_and(|work| work.local_attached(handle))
            }
            NotificationSource::Cloud(handle) => {
                self.function_cloud_candidate(did, id, false).is_ok()
                    && self.current_notifications(handle)
                    && work
                        .as_ref()
                        .is_some_and(|work| work.cloud_attached(handle))
            }
        }
    }
}

fn activate(context: &Context, work: &DeviceWork, id: &str, source: NotificationSource) {
    let did = &work.key.id;
    work.install_observer(id, source);
    context.update_device_availability(did);
    context.refresh_group_states(did);
    context.note_notification_switch(did, "notification subscriptions ready; source activated");
}
pub(super) async fn selections(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let listener = context.changed.listen();
        let works = context
            .live
            .borrow()
            .devices
            .values()
            .filter_map(|slot| slot.admitted.clone())
            .collect::<Vec<_>>();
        for work in works {
            reconcile_source(&context, work);
        }
        if let Some(session) = context.notifications.borrow().clone() {
            reconcile_online(&context, session);
        }
        listener.await;
    }
}
fn reconcile_source(context: &Rc<Context>, work: Rc<DeviceWork>) {
    let did = work.key.id.clone();
    if work.life.is_closed() {
        return;
    }
    work.reconcile_state_reads(context);
    let retry = work.take_subscription_retry();
    let mut keep_local = false;
    let mut keep_cloud = false;
    for id in work.channels() {
        if let Some(old) = work.function_observer(&id)
            && !context.source_valid_for(&did, &id, &old)
        {
            work.revoke_observer(&id);
            context.update_device_availability(&did);
            context.refresh_group_states(&did);
            context.update_routing("notification source access conditions invalidated");
        }
        let desired = context.notification_source(&did, &id);
        if let Some(desired) = desired {
            keep_local |= matches!(desired, NotificationSource::Local(_));
            keep_cloud |= matches!(desired, NotificationSource::Cloud(_));
            let active = work.function_observer(&id);
            let same = active.as_ref().is_some_and(|active| active.same(&desired));
            if !same && work.source_ready(&id, &desired) {
                activate(context, &work, &id, desired.clone());
            } else if !same || retry {
                prepare_source(context, work.clone(), desired);
            }
        }
        if let Some(active) = work.function_observer(&id) {
            keep_local |= matches!(active, NotificationSource::Local(_));
            keep_cloud |= matches!(active, NotificationSource::Cloud(_));
        }
    }
    if !keep_local && work.local_preparation_active() {
        work.retire_local_notifications();
    }
    if !keep_cloud && work.cloud_preparation_active() {
        work.retire_cloud_notifications();
    }
}

fn prepare_source(context: &Rc<Context>, work: Rc<DeviceWork>, source: NotificationSource) {
    match source {
        NotificationSource::Local(_) => {
            work.request_local_subscriptions();
        }
        NotificationSource::Cloud(handle) => {
            if let Some(preparation) = work.begin_cloud_preparation(&handle) {
                let session = context.notifications.borrow().as_ref().unwrap().clone();
                session.tasks.spawn(prepare_properties(
                    context.clone(),
                    work.clone(),
                    handle,
                    session.key.token.clone(),
                    session.subscriptions.clone(),
                    preparation,
                ));
            }
        }
    }
}
