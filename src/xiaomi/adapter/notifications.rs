use super::{
    model::AdapterError,
    routing::PathPriority,
    state::{Backoff, Context, Lifetime},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::{
        auth::FailureReason,
        central::SessionHandle,
        cloud::{NotificationDriver, NotificationError, NotificationEvent, NotificationHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::support::NotificationConnector;
#[derive(Clone)]
pub(super) enum NotificationSource {
    Local(SessionHandle),
    Cloud(NotificationHandle),
}
impl NotificationSource {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => a.same_connection(b),
            (Self::Cloud(a), Self::Cloud(b)) => a.same_connection(b),
            _ => false,
        }
    }
    fn invalidate(&self, did: &str) {
        match self {
            Self::Local(h) => h.invalidate_received_properties(did),
            Self::Cloud(h) => h.invalidate_received_properties(did),
        }
    }
    fn retire(&self, work: &DeviceWork, did: &str) {
        match self {
            Self::Local(handle) => {
                drop(handle.unsubscribe_properties(did));
                work.local_generation.set(work.local_generation.get() + 1);
                work.subscribed.borrow_mut().clear();
            }
            Self::Cloud(handle) => {
                drop(handle.unsubscribe_properties(did));
                work.cloud_generation.set(work.cloud_generation.get() + 1);
                work.cloud_subscribed.borrow_mut().clear();
            }
        }
    }
}
pub(super) struct CloudSession {
    pub(super) handle: NotificationHandle,
    key: ConnectionKey,
    pub(super) tasks: TaskSet,
    online: RefCell<BTreeMap<String, Rc<OnlinePreparation>>>,
    pub(super) subscriptions: Rc<SubscriptionBudget>,
}

// Online and property handshakes share capacity, leaving transport slots for cleanup.
const SUBSCRIPTION_LIMIT: usize = 16;

#[derive(Default)]
pub(super) struct SubscriptionBudget {
    in_flight: Cell<usize>,
    released: Event,
}
pub(super) struct SubscriptionPermit<'a>(&'a SubscriptionBudget);
impl SubscriptionBudget {
    pub(super) async fn acquire(
        &self,
        changed: &Event,
        life: &Lifetime,
        current: &impl Fn() -> bool,
    ) -> Option<SubscriptionPermit<'_>> {
        loop {
            let released = self.released.listen();
            let changed = changed.listen();
            if life.is_closed() || !current() {
                return None;
            }
            if self.in_flight.get() < SUBSCRIPTION_LIMIT {
                self.in_flight.set(self.in_flight.get() + 1);
                return Some(SubscriptionPermit(self));
            }
            future::or(released, future::or(changed, life.ended())).await;
        }
    }
}
impl Drop for SubscriptionPermit<'_> {
    fn drop(&mut self) {
        self.0.in_flight.set(self.0.in_flight.get() - 1);
        // A retired waiter must not consume the only wakeup for an available slot.
        self.0.released.notify(usize::MAX);
    }
}
struct OnlinePreparation {
    condition: u64,
    failed: Cell<bool>,
}
#[derive(Clone, Eq, PartialEq)]
struct ConnectionKey {
    uid: String,
    token: String,
    client: String,
    home: String,
}
fn connection_key(context: &Context) -> Option<ConnectionKey> {
    if !context.cloud_authenticated() {
        return None;
    }
    let live = context.live.borrow();
    let credentials = live.credentials.as_ref()?;
    let scope = live.scope.as_ref()?;
    Some(ConnectionKey {
        uid: credentials.uid.clone(),
        token: credentials.tokens.access_token.clone(),
        client: credentials.oauth_client_uuid.clone(),
        home: scope.home_id.clone(),
    })
}
fn connection_credentials(context: &Context) -> Option<(ConnectionKey, XiaomiRecord)> {
    let key = connection_key(context)?;
    let credentials = context.live.borrow().credentials.as_ref().unwrap().clone();
    Some((key, credentials))
}

impl Context {
    pub(super) fn active_notification(&self, did: &str) -> Option<super::model::PathKind> {
        if self.is_virtual_group(did) {
            self.control_path(did)?;
            let live = self.live.borrow();
            let group = live.devices[did].effective_group.as_ref().unwrap();
            return group
                .members
                .iter()
                .try_fold(super::model::PathKind::Local, |path, member| {
                    let member_path = self.active_notification(member)?;
                    Some(if member_path == super::model::PathKind::Cloud {
                        member_path
                    } else {
                        path
                    })
                });
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone())?;
        let active = work.active.borrow();
        let source = active
            .as_ref()
            .filter(|source| self.source_valid(did, source))?;
        Some(match source {
            NotificationSource::Local(_) => super::model::PathKind::Local,
            NotificationSource::Cloud(_) => super::model::PathKind::Cloud,
        })
    }
    pub(super) fn current_notifications(&self, handle: &NotificationHandle) -> bool {
        self.notifications.borrow().as_ref().is_some_and(|session| {
            session.handle.same_connection(handle)
                && connection_key(self).is_some_and(|key| key == session.key)
        })
    }
    pub(super) fn notification_source(&self, did: &str) -> Option<NotificationSource> {
        // Virtual groups derive their state from ordinary member subscriptions.
        if self.is_virtual_group(did) {
            return None;
        }
        let local = self
            .local_candidate(did, true)
            .ok()
            .map(NotificationSource::Local);
        let cloud = if self.cloud_candidate(did, false).is_ok() {
            self.notifications
                .borrow()
                .as_ref()
                .filter(|session| self.current_notifications(&session.handle))
                .map(|session| NotificationSource::Cloud(session.handle.clone()))
        } else {
            None
        };
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        let local_failed = work.as_ref().is_some_and(|work| work.local_exhausted.get());
        let cloud_failed = work.as_ref().is_some_and(|work| work.cloud_exhausted.get());
        let local = local.filter(|_| !local_failed);
        let cloud = cloud.filter(|_| !cloud_failed);
        match self.priority() {
            PathPriority::LocalFirst => local.or(cloud),
            PathPriority::CloudFirst => cloud.or(local),
        }
    }
    fn source_valid(&self, did: &str, source: &NotificationSource) -> bool {
        if self.is_virtual_group(did) {
            return false;
        }
        match source {
            NotificationSource::Local(handle) => self
                .local_candidate(did, true)
                .is_ok_and(|candidate| candidate.same_connection(handle)),
            NotificationSource::Cloud(handle) => {
                self.cloud_candidate(did, false).is_ok() && self.current_notifications(handle)
            }
        }
    }
    pub(super) fn property_value_event(
        &self,
        source: &NotificationSource,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: &serde_json::Value,
        cached_report: bool,
    ) {
        if !self.source_valid(did, source) {
            return;
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        if let Some(work) = work
            && work
                .active
                .borrow()
                .as_ref()
                .is_some_and(|active| active.same(source))
            && work.notify_value(
                binding,
                value,
                cached_report,
                self.is_independent_light(did),
            )
            && !self.is_group(did)
        {
            self.refresh_group_states(did);
        }
    }
    fn online_event(&self, handle: &NotificationHandle, did: &str, online: bool) {
        if !self.current_notifications(handle) {
            return;
        }
        if let Some(slot) = self.live.borrow_mut().devices.get_mut(did) {
            if let Some(access) = &mut slot.cloud_access {
                if online != access.online
                    && let Some(work) = &slot.admitted
                {
                    work.retry_subscriptions();
                }
                access.online = online;
            } else {
                self.request_sync();
                return;
            }
        } else {
            return;
        }
        // Online reports only update this path's access, never HTTP/auth suspension.
        self.update_availability(did);
        self.update_routing("cloud online conditions changed");
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
    }

    /// Retire every notification subscription owned by a device whose slot is
    /// leaving the confirmed set about to be dropped. Mirrors the desired `None`
    /// branches of `reconcile_source` without a new mechanism.
    pub(super) fn retire_device_notifications(&self, did: &str) {
        let Some(work) = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone())
        else {
            return;
        };
        let active = work.active.borrow_mut().take();
        let local_done = matches!(active, Some(NotificationSource::Local(_)));
        let cloud_done = matches!(active, Some(NotificationSource::Cloud(_)));
        if let Some(active) = &active {
            // `NotificationSource::retire` already unsubscribes properties and
            // advances that source's generation with a receipt gate.
            active.retire(&work, did);
        }
        if !local_done && let Some(session) = self.live.borrow().session.as_ref() {
            // Retire local properties including Preparing registrations, then
            // advance the generation so a late SUBACK cannot reinstate them.
            drop(session.handle.unsubscribe_properties(did));
            work.local_generation.set(work.local_generation.get() + 1);
            work.subscribed.borrow_mut().clear();
        }
        if let Some(session) = self.notifications.borrow().as_ref() {
            // The cloud session also owns online notifications for this DID;
            // both are retired through the existing device-level unsubscribe.
            drop(session.handle.unsubscribe_device(did));
            if !cloud_done {
                work.cloud_generation.set(work.cloud_generation.get() + 1);
                work.cloud_subscribed.borrow_mut().clear();
            }
        }
    }
}
fn ready(work: &DeviceWork, source: &NotificationSource) -> bool {
    let subscribed = match source {
        NotificationSource::Local(_) => work.subscribed.borrow(),
        NotificationSource::Cloud(_) => work.cloud_subscribed.borrow(),
    };
    work.bindings()
        .iter()
        .all(|b| subscribed.contains(&(b.siid, b.piid)))
}
fn activate(context: &Context, work: &DeviceWork, source: NotificationSource) {
    let did = &work.key.id;
    // Invalidate received QoS2 values before granting delivery; keep handshakes and ready SUBACKs.
    source.invalidate(did);
    if let Some(old) = work.active.borrow_mut().take() {
        old.retire(work, did);
    }
    *work.active.borrow_mut() = Some(source);
    work.source_changed();
    context.refresh_group_states(did);
    context.note_notification_switch(did, "notification subscriptions ready; source activated");
}
async fn selections(context: Rc<Context>) -> Result<(), AdapterError> {
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
    let old = work.active.borrow().clone();
    if let Some(old) = old
        && !context.source_valid(&did, &old)
    {
        old.retire(&work, &did);
        work.active.borrow_mut().take();
        work.source_changed();
        context.refresh_group_states(&did);
        context.update_routing("notification source access conditions invalidated");
    }
    let desired = context.notification_source(&did);
    // Keep the desired preparation and the active source while handing over to it.
    let keep_local = matches!(desired, Some(NotificationSource::Local(_)))
        || (desired.is_some()
            && matches!(*work.active.borrow(), Some(NotificationSource::Local(_))));
    let keep_cloud = matches!(desired, Some(NotificationSource::Cloud(_)))
        || (desired.is_some()
            && matches!(*work.active.borrow(), Some(NotificationSource::Cloud(_))));
    if !keep_local
        && (!work.subscribed.borrow().is_empty() || work.subscribe_running.get())
        && let Some(session) = context.live.borrow().session.as_ref()
    {
        NotificationSource::Local(session.handle.clone()).retire(&work, &did);
    }
    if !keep_cloud
        && (!work.cloud_subscribed.borrow().is_empty() || work.cloud_running.get())
        && let Some(session) = context.notifications.borrow().as_ref()
    {
        NotificationSource::Cloud(session.handle.clone()).retire(&work, &did);
        work.cloud_running.set(false);
    }
    let Some(desired) = desired else { return };

    if work
        .active
        .borrow()
        .as_ref()
        .is_some_and(|active| active.same(&desired))
    {
        return;
    }
    if ready(&work, &desired) {
        activate(context, &work, desired);
        return;
    }
    match desired {
        NotificationSource::Local(_) => {
            work.request_local_subscriptions();
        }
        NotificationSource::Cloud(handle) => {
            if !work.cloud_running.replace(true) {
                let session = context.notifications.borrow().as_ref().unwrap().clone();
                session.tasks.spawn(prepare_properties(
                    context.clone(),
                    work.clone(),
                    handle,
                    session.key.token.clone(),
                    session.subscriptions.clone(),
                    work.cloud_generation.get(),
                ));
            }
        }
    }
}
fn online_work(context: &Context, did: &str) -> Option<Rc<DeviceWork>> {
    let live = context.live.borrow();
    let slot = live.devices.get(did)?;
    if slot
        .effective_group
        .as_ref()
        .is_some_and(|group| !group.is_physical)
    {
        return None;
    }
    let work = slot.admitted.as_ref()?;
    if work.life.is_closed() || slot.cloud_forbidden || slot.cloud_access.is_none() {
        return None;
    }
    Some(work.clone())
}
fn reconcile_online(context: &Rc<Context>, session: Rc<CloudSession>) {
    let works = context
        .live
        .borrow()
        .devices
        .keys()
        .filter_map(|did| online_work(context, did).map(|work| (did.clone(), work)))
        .collect::<BTreeMap<_, _>>();
    session.online.borrow_mut().retain(|did, _| {
        if works.contains_key(did) {
            true
        } else {
            // Only online ownership is reconciled here; exact property
            // member subscriptions have their own retirement lifecycles.
            drop(session.handle.unsubscribe_online(did));
            false
        }
    });
    for (did, work) in works {
        let state = session.online.borrow().get(&did).cloned();
        if state.as_ref().is_some_and(|state| {
            !state.failed.get() || state.condition == work.condition_generation.get()
        }) {
            continue;
        }
        let state = Rc::new(OnlinePreparation {
            condition: work.condition_generation.get(),
            failed: Cell::new(false),
        });
        session
            .online
            .borrow_mut()
            .insert(did.clone(), state.clone());
        let context = context.clone();
        let handle = session.handle.clone();
        let token = session.key.token.clone();
        session.tasks.spawn(prepare_online(
            context,
            work,
            handle,
            token,
            did,
            state,
            session.subscriptions.clone(),
        ));
    }
}
async fn prepare_online(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
    did: String,
    state: Rc<OnlinePreparation>,
    subscriptions: Rc<SubscriptionBudget>,
) -> Result<(), AdapterError> {
    let current = || {
        context.current_notifications(&handle)
            && online_work(&context, &did).is_some_and(|current| Rc::ptr_eq(&current, &work))
            && context
                .notifications
                .borrow()
                .as_ref()
                .is_some_and(|session| {
                    session
                        .online
                        .borrow()
                        .get(&did)
                        .is_some_and(|current| Rc::ptr_eq(current, &state))
                })
    };
    for attempt in 0..4 {
        let Some(permit) = subscriptions
            .acquire(&context.changed, &work.life, &current)
            .await
        else {
            break;
        };
        if !current() {
            break;
        }
        let result = handle.subscribe_online(&did).await;
        drop(permit);
        if !current() {
            break;
        }
        match result {
            Ok(()) => return Ok(()),
            Err(NotificationError::Unauthorized) => {
                context.token_rejected(
                    &token,
                    FailureReason::CloudNotification(NotificationError::Unauthorized),
                )?;
                break;
            }
            Err(error) => {
                log::warn!("{did} cloud online subscribe: {error}");
                if attempt < 3 {
                    context
                        .clock
                        .wait_until(context.clock.now() + Duration::from_secs(1))
                        .await;
                }
            }
        }
    }
    state.failed.set(true);
    Ok(())
}
async fn prepare_properties(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
    subscriptions: Rc<SubscriptionBudget>,
    preparation: u64,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let current = || {
        !work.life.is_closed()
            && work.cloud_generation.get() == preparation
            && context.current_notifications(&handle)
            && context.cloud_candidate(&did, false).is_ok()
    };
    let mut pending = work
        .bindings()
        .into_iter()
        .filter(|binding| {
            !work
                .cloud_subscribed
                .borrow()
                .contains(&(binding.siid, binding.piid))
        })
        .collect::<Vec<_>>();
    'attempts: for attempt in 0..4 {
        let mut failed = Vec::new();
        let mut problem = None;
        for binding in pending {
            let Some(permit) = subscriptions
                .acquire(&context.changed, &work.life, &current)
                .await
            else {
                break 'attempts;
            };
            if !current() {
                break 'attempts;
            }
            let result = handle.subscribe_property(&did, binding).await;
            drop(permit);
            if !current() {
                break 'attempts;
            }
            match result {
                Ok(()) => {
                    work.cloud_subscribed
                        .borrow_mut()
                        .insert((binding.siid, binding.piid));
                }
                Err(NotificationError::Unauthorized) => {
                    context.token_rejected(
                        &token,
                        FailureReason::CloudNotification(NotificationError::Unauthorized),
                    )?;
                    break 'attempts;
                }
                Err(error) => {
                    problem = Some(format!(
                        "{did} cloud subscribe {}.{}: {error}",
                        binding.siid, binding.piid
                    ));
                    failed.push(binding);
                }
            }
        }
        if failed.is_empty() || attempt == 3 {
            work.cloud_exhausted.set(problem.is_some());
            *work.cloud_problem.borrow_mut() = problem;
            work.report(&context, &did);
            break;
        }
        pending = failed;
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_secs(1))
            .await;
    }
    if work.cloud_generation.get() == preparation {
        work.cloud_running.set(false);
    }
    context.changed.notify(usize::MAX);
    Ok(())
}
#[cfg(test)]
use tests::support::connect;
#[cfg(not(test))]
async fn connect(
    context: &Context,
    credentials: &XiaomiRecord,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    let _ = context;
    crate::xiaomi::cloud::connect_notifications(credentials).await
}
async fn changed_credentials(context: &Context, key: &ConnectionKey) {
    loop {
        let changed = context.changed.listen();
        let credentials = context.credentials_changed.listen();
        if connection_key(context).as_ref() != Some(key) {
            return;
        }
        future::or(
            async {
                changed.await;
            },
            async {
                credentials.await;
            },
        )
        .await;
    }
}
async fn connections(context: Rc<Context>) -> Result<(), AdapterError> {
    let mut backoff = Backoff::new(10, 600);
    loop {
        let changed = context.changed.listen();
        let credentials = context.credentials_changed.listen();
        let Some((key, record)) = connection_credentials(&context) else {
            future::or(
                async {
                    changed.await;
                },
                async {
                    credentials.await;
                },
            )
            .await;
            continue;
        };
        let connecting = future::or(async { Some(connect(&context, &record).await) }, async {
            changed_credentials(&context, &key).await;
            None
        })
        .await;
        drop(record);
        let Some(result) = connecting else {
            backoff = Backoff::new(10, 600);
            continue;
        };
        let result = match result {
            Ok((handle, driver)) => {
                if connection_key(&context).as_ref() != Some(&key) {
                    handle.close();
                    continue;
                }
                backoff = Backoff::new(10, 600);
                let session = Rc::new(CloudSession {
                    handle: handle.clone(),
                    key: key.clone(),
                    tasks: TaskSet::default(),
                    online: RefCell::new(BTreeMap::new()),
                    subscriptions: Rc::new(SubscriptionBudget::default()),
                });
                *context.notifications.borrow_mut() = Some(session.clone());
                for work in context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.admitted.as_ref())
                {
                    work.cloud_disconnected();
                    work.retry_subscriptions();
                    // HTTP feedback must also recover when property
                    // subscriptions never became active in the old session.
                    if work.active.borrow().is_none() && !context.is_virtual_group(&work.key.id) {
                        work.refresh();
                    }
                }
                context.update_all_availability();
                context.changed.notify(usize::MAX);
                context.update_routing("cloud notification connection established");
                let events = context.clone();
                let event_handle = handle.clone();
                let driving = async {
                    let result = driver
                        .run(move |event| match event {
                            NotificationEvent::Online { did, online } => {
                                events.online_event(&event_handle, &did, online)
                            }
                            NotificationEvent::PropertyValue {
                                did,
                                binding,
                                value,
                            } => events.property_value_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                &value,
                                false,
                            ),
                            NotificationEvent::PropertySnapshot {
                                did,
                                binding,
                                value,
                            } => events.property_value_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                &value,
                                true,
                            ),
                        })
                        .await
                        .err();
                    Ok::<_, AdapterError>(result)
                };
                let result = future::or(
                    driving,
                    future::or(
                        async {
                            changed_credentials(&context, &key).await;
                            Ok(None)
                        },
                        async {
                            session.tasks.run().await?;
                            Ok(None)
                        },
                    ),
                )
                .await?;
                context.notifications.borrow_mut().take();
                handle.close();
                session.tasks.clear();
                for work in context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.admitted.as_ref())
                {
                    work.cloud_disconnected();
                    work.retry_subscriptions();
                }
                context.update_all_availability();
                context.changed.notify(usize::MAX);
                context.update_routing("cloud notification connection ended");
                result
            }
            Err(error) => Some(error),
        };
        if let Some(error) = result {
            if connection_key(&context).as_ref() == Some(&key) {
                if error == NotificationError::Unauthorized {
                    context.token_rejected(&key.token, FailureReason::CloudNotification(error))?;
                } else {
                    log::warn!("Cloud notifications unavailable: {error}");
                }
            }
        } else {
            backoff = Backoff::new(10, 600);
            continue;
        }
        let due = context.clock.now() + backoff.failed();
        future::or(
            context.clock.wait_until(due),
            changed_credentials(&context, &key),
        )
        .await;
        if connection_key(&context).as_ref() != Some(&key) {
            backoff = Backoff::new(10, 600);
        }
    }
}
pub(super) async fn run(context: Rc<Context>) -> Result<(), AdapterError> {
    future::or(selections(context.clone()), connections(context)).await
}
