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
        let mut routes = context.device_routes_for(works.iter().map(|work| work.key.id.as_str()));
        for work in works {
            let device_routes = routes.remove(&work.key.id).unwrap();
            reconcile_source(&context, work, device_routes);
        }
        if let Some(session) = context.notifications.borrow().clone() {
            reconcile_online(&context, session);
        }
        listener.await;
    }
}
fn reconcile_source(
    context: &Rc<Context>,
    work: Rc<DeviceWork>,
    routes: super::super::routing::DeviceRoutes,
) {
    let did = work.key.id.clone();
    if work.life.is_closed() {
        return;
    }
    work.reconcile_state_reads(&routes);
    let retry = work.take_subscription_retry();
    let mut keep_local = false;
    let mut keep_cloud = false;
    for id in work.channels() {
        // A valid standby retains its own preparation and diagnostics across a
        // priority handover; only loss of access withdraws its subscriptions.
        for source in routes.functions[&id].notification.sources() {
            match source {
                NotificationSource::Local(_) => keep_local = true,
                NotificationSource::Cloud(_) => keep_cloud = true,
            }
        }
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
            let active = work.function_observer(&id);
            let same = active.as_ref().is_some_and(|active| active.same(&desired));
            if !same && work.source_ready(&id, &desired) {
                activate(context, &work, &id, desired.clone());
            } else if !same || retry {
                prepare_source(context, work.clone(), desired);
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
            if let Some(preparation) =
                work.begin_preparation(&NotificationSource::Cloud(handle.clone()))
            {
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
