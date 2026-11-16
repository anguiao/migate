use super::*;

pub(in crate::xiaomi::adapter) fn connection_matches(
    session: &CloudSession,
    record: &XiaomiRecord,
    key: &super::super::routing::HaKey,
    home: &str,
) -> bool {
    let Some(authorization) = record.ha_authorization() else {
        return false;
    };
    session.key.uid == record.uid
        && session.key.generation == key.generation
        && session.key.token == authorization.tokens.access_token
        && session.key.client == record.ha.client_uuid
        && session.key.home == home
}

impl Context {
    pub(in crate::xiaomi::adapter) fn current_notifications(
        &self,
        handle: &NotificationHandle,
    ) -> bool {
        self.notification_handle_at(self.clock.unix_now())
            .is_some_and(|current| current.same_connection(handle))
    }
    pub(in crate::xiaomi::adapter) fn notification_source(
        &self,
        did: &str,
        id: &str,
    ) -> Option<NotificationSource> {
        self.device_routes(did)
            .functions
            .get(id)?
            .notification
            .desired
            .clone()
    }
    pub(in crate::xiaomi::adapter) fn source_valid_for(
        &self,
        did: &str,
        id: &str,
        source: &NotificationSource,
    ) -> bool {
        self.device_routes(did)
            .functions
            .get(id)
            .is_some_and(|function| function.notification.valid(source))
    }
}

fn activate(context: &Context, work: &DeviceWork, id: &str, source: NotificationSource) {
    let did = &work.key.id;
    work.install_observer(id, source);
    context.update_device_availability(did);
    context.refresh_group_states(did);
    context.note_notification_switch(
        did,
        id,
        "notification subscriptions ready; source activated",
    );
}
pub(super) async fn selections(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let listener = context.changed.listen();
        let works = context
            .directory
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
    let routes = context.device_routes(&did);
    work.reconcile_state_reads(&routes);
    let retry = work.take_subscription_retry();
    let mut keep_local = false;
    let mut keep_cloud = false;
    for id in work.channels() {
        if let Some(old) = work.function_observer(&id)
            && !routes.functions[&id].notification.valid(&old)
        {
            work.revoke_observer(&id);
            context.update_device_availability(&did);
            context.refresh_group_states(&did);
            context
                .update_device_routing(&did, "notification source access conditions invalidated");
        }
        let desired = routes.functions[&id].notification.desired.clone();
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
        if let Some((binding, true)) = work
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .and_then(crate::xiaomi::capability::ParsedFunction::relay_mode)
        {
            if let Ok(handle) = &routes.hub_push {
                let source = NotificationSource::Local(handle.clone());
                keep_local |= routes.functions[&id].notification.valid(&source)
                    && work.source_binding_ready(&source, binding);
            }
            if let Some(handle) = context
                .notifications
                .borrow()
                .as_ref()
                .map(|session| session.handle.clone())
            {
                let source = NotificationSource::Cloud(handle);
                keep_cloud |= routes.functions[&id].notification.valid(&source)
                    && work.source_binding_ready(&source, binding);
            }
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
                    session.key.clone(),
                    session.subscriptions.clone(),
                    preparation,
                ));
            }
        }
    }
}
