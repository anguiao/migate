use super::{
    model::AdapterError,
    routing::PathPriority,
    state::{Backoff, Context},
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
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum NotificationConnector {
    Disabled,
    Plaintext(std::net::SocketAddr),
}
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
    credentials: XiaomiRecord,
    key: ConnectionKey,
    tasks: TaskSet,
    online: RefCell<BTreeMap<String, Rc<OnlinePreparation>>>,
}
struct OnlinePreparation {
    condition: u64,
    ready: Cell<bool>,
    running: Cell<bool>,
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
    pub(super) fn current_notifications(&self, handle: &NotificationHandle) -> bool {
        self.cloud_authenticated()
            && self.notifications.borrow().as_ref().is_some_and(|session| {
                session.handle.same_connection(handle)
                    && connection_key(self).is_some_and(|key| key == session.key)
            })
    }
    pub(super) fn notification_source(&self, did: &str) -> Option<NotificationSource> {
        let local = self
            .local_candidate(did, true)
            .map(NotificationSource::Local);
        let cloud = if self.cloud_candidate(did, false) {
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
            .and_then(|slot| slot.work.clone());
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
        match source {
            NotificationSource::Local(handle) => self
                .local_candidate(did, true)
                .is_some_and(|candidate| candidate.same_connection(handle)),
            NotificationSource::Cloud(handle) => {
                self.cloud_candidate(did, false) && self.current_notifications(handle)
            }
        }
    }
    pub(super) fn property_event(
        &self,
        source: &NotificationSource,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: bool,
    ) {
        if !self.source_valid(did, source) {
            return;
        }
        let work = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        if let Some(work) = work
            && work
                .active
                .borrow()
                .as_ref()
                .is_some_and(|active| active.same(source))
        {
            work.notify(binding, value);
        }
    }
    fn online_event(&self, handle: &NotificationHandle, did: &str, online: bool) {
        if !self.current_notifications(handle) {
            return;
        }
        if let Some(slot) = self.live.borrow_mut().devices.get_mut(did) {
            if let Some(access) = &mut slot.cloud_access {
                if online
                    && !access.online
                    && let Some(work) = &slot.work
                {
                    work.cloud_exhausted.set(false);
                    work.condition_generation
                        .set(work.condition_generation.get() + 1);
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
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
    }
}
fn ready(work: &DeviceWork, source: &NotificationSource) -> bool {
    let bindings = work.bindings();
    match source {
        NotificationSource::Local(_) => bindings
            .iter()
            .all(|b| work.subscribed.borrow().contains(&(b.siid, b.piid))),
        NotificationSource::Cloud(_) => bindings
            .iter()
            .all(|b| work.cloud_subscribed.borrow().contains(&(b.siid, b.piid))),
    }
}
fn activate(work: &DeviceWork, source: NotificationSource) {
    let did = &work.record.physical.key().id;
    // Invalidate received QoS2 values before granting delivery; keep handshakes and ready SUBACKs.
    source.invalidate(did);
    if let Some(old) = work.active.borrow_mut().take() {
        old.retire(work, did);
    }
    *work.active.borrow_mut() = Some(source);
    work.source_changed();
}
async fn selections(context: Rc<Context>) -> Result<(), AdapterError> {
    loop {
        let listener = context.changed.listen();
        let works = context
            .live
            .borrow()
            .devices
            .values()
            .filter_map(|slot| slot.work.clone())
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
    let did = work.record.physical.key().id.clone();
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
    }
    let Some(desired) = context.notification_source(&did) else {
        if (!work.subscribed.borrow().is_empty() || work.subscribe_running.get())
            && let Some(handle) = context
                .live
                .borrow()
                .session
                .as_ref()
                .map(|session| session.handle.clone())
        {
            NotificationSource::Local(handle).retire(&work, &did);
        }
        if (!work.cloud_subscribed.borrow().is_empty() || work.cloud_running.get())
            && let Some(handle) = context
                .notifications
                .borrow()
                .as_ref()
                .map(|session| session.handle.clone())
        {
            NotificationSource::Cloud(handle).retire(&work, &did);
            work.cloud_running.set(false);
        }
        return;
    };
    // Release a no-longer-needed preparation; retain only the active source during handoff.
    match &desired {
        NotificationSource::Cloud(_)
            if !matches!(*work.active.borrow(), Some(NotificationSource::Local(_)))
                && (!work.subscribed.borrow().is_empty() || work.subscribe_running.get()) =>
        {
            if let Some(handle) = context
                .live
                .borrow()
                .session
                .as_ref()
                .map(|session| session.handle.clone())
            {
                NotificationSource::Local(handle).retire(&work, &did);
            }
        }
        NotificationSource::Local(_)
            if !matches!(*work.active.borrow(), Some(NotificationSource::Cloud(_)))
                && (!work.cloud_subscribed.borrow().is_empty() || work.cloud_running.get()) =>
        {
            if let Some(handle) = context
                .notifications
                .borrow()
                .as_ref()
                .map(|session| session.handle.clone())
            {
                NotificationSource::Cloud(handle).retire(&work, &did);
                work.cloud_running.set(false);
            }
        }
        _ => {}
    }

    if work
        .active
        .borrow()
        .as_ref()
        .is_some_and(|active| active.same(&desired))
    {
        return;
    }
    if ready(&work, &desired) {
        activate(&work, desired);
        return;
    }
    match desired {
        NotificationSource::Local(_) => {
            if !work.local_exhausted.get() {
                work.request_local_subscriptions();
            }
        }
        NotificationSource::Cloud(handle) => {
            if !work.cloud_running.replace(true) && !work.cloud_exhausted.get() {
                let session = context.notifications.borrow().as_ref().unwrap().clone();
                session.tasks.spawn(prepare_properties(
                    context.clone(),
                    work.clone(),
                    handle,
                    session.credentials.tokens.access_token.clone(),
                ));
            } else if work.cloud_exhausted.get() {
                work.cloud_running.set(false);
            }
        }
    }
}
fn reconcile_online(context: &Rc<Context>, session: Rc<CloudSession>) {
    let works = context
        .live
        .borrow()
        .devices
        .iter()
        .filter(|(_, slot)| {
            slot.admitted.is_some()
                && slot.blocked.is_none()
                && !slot.cloud_forbidden
                && slot.cloud_access.is_some()
        })
        .filter_map(|(did, slot)| slot.work.clone().map(|work| (did.clone(), work)))
        .collect::<BTreeMap<_, _>>();
    let registered = session.online.borrow().keys().cloned().collect::<Vec<_>>();
    for did in registered {
        if !works.contains_key(&did) {
            drop(session.handle.unsubscribe_device(&did));
            session.online.borrow_mut().remove(&did);
        }
    }
    for (did, work) in works {
        let state = session.online.borrow().get(&did).cloned();
        if state.as_ref().is_some_and(|state| {
            state.ready.get()
                || state.running.get()
                || state.condition == work.condition_generation.get()
        }) {
            continue;
        }
        let state = Rc::new(OnlinePreparation {
            condition: work.condition_generation.get(),
            ready: Cell::new(false),
            running: Cell::new(true),
        });
        session
            .online
            .borrow_mut()
            .insert(did.clone(), state.clone());
        let context = context.clone();
        let handle = session.handle.clone();
        let token = session.credentials.tokens.access_token.clone();
        session
            .tasks
            .spawn(prepare_online(context, work, handle, token, did, state));
    }
}
async fn prepare_online(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
    did: String,
    state: Rc<OnlinePreparation>,
) -> Result<(), AdapterError> {
    for attempt in 0..4 {
        if work.life.is_closed() || !context.current_notifications(&handle) {
            state.running.set(false);
            return Ok(());
        }
        match handle.subscribe_online(&did).await {
            Ok(()) => {
                if !work.life.is_closed() && context.current_notifications(&handle) {
                    state.ready.set(true);
                }
                break;
            }
            Err(NotificationError::Unauthorized) if context.current_notifications(&handle) => {
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
    state.running.set(false);
    Ok(())
}
async fn prepare_properties(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
) -> Result<(), AdapterError> {
    let generation = work.record.binding_generation.get();
    let preparation = work.cloud_generation.get();
    let did = work.record.physical.key().id.clone();
    let current = || {
        !work.life.is_closed()
            && work.record.binding_generation.get() == generation
            && work.cloud_generation.get() == preparation
            && context.current_notifications(&handle)
            && context.cloud_candidate(&did, false)
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
    for attempt in 0..4 {
        let mut failed = Vec::new();
        let mut problem = None;
        for binding in pending {
            if !current() {
                if work.cloud_generation.get() == preparation {
                    work.cloud_running.set(false);
                }
                return Ok(());
            }
            match handle.subscribe_property(&did, binding).await {
                Ok(()) if current() => {
                    work.cloud_subscribed
                        .borrow_mut()
                        .insert((binding.siid, binding.piid));
                }
                Ok(()) => {}
                Err(NotificationError::Unauthorized) if current() => {
                    context.token_rejected(
                        &token,
                        FailureReason::CloudNotification(NotificationError::Unauthorized),
                    )?;
                    if work.cloud_generation.get() == preparation {
                        work.cloud_running.set(false);
                    }
                    return Ok(());
                }
                Err(error) if current() => {
                    problem = Some(format!(
                        "{did} cloud subscribe {}.{}: {error}",
                        binding.siid, binding.piid
                    ));
                    failed.push(binding);
                }
                Err(_) => {}
            }
        }
        if !current() {
            if work.cloud_generation.get() == preparation {
                work.cloud_running.set(false);
            }
            return Ok(());
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
async fn connect(
    context: &Context,
    credentials: &XiaomiRecord,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    #[cfg(test)]
    {
        match context.notification_connector.get() {
            NotificationConnector::Disabled => future::pending().await,
            NotificationConnector::Plaintext(address) => {
                crate::xiaomi::cloud::notification_test_support::connect(
                    address,
                    credentials,
                    Duration::from_millis(150),
                    None,
                )
                .await
            }
        }
    }
    #[cfg(not(test))]
    {
        let _ = context;
        crate::xiaomi::cloud::connect_notifications(credentials).await
    }
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
                    credentials: record.clone(),
                    key: key.clone(),
                    tasks: TaskSet::default(),
                    online: RefCell::new(BTreeMap::new()),
                });
                *context.notifications.borrow_mut() = Some(session.clone());
                for work in context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.work.as_ref())
                {
                    work.cloud_disconnected();
                }
                context.changed.notify(usize::MAX);
                let events = context.clone();
                let event_handle = handle.clone();
                let driving = async {
                    let result = driver
                        .run(move |event| match event {
                            NotificationEvent::Property {
                                did,
                                binding,
                                value,
                            } => events.property_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                value,
                            ),
                            NotificationEvent::Online { did, online } => {
                                events.online_event(&event_handle, &did, online)
                            }
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
                    .filter_map(|slot| slot.work.as_ref())
                {
                    work.cloud_disconnected();
                }
                context.changed.notify(usize::MAX);
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
