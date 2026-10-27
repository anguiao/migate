use super::*;

pub(in crate::xiaomi::adapter) struct CloudSession {
    pub(in crate::xiaomi::adapter) handle: NotificationHandle,
    pub(super) key: ConnectionKey,
    pub(in crate::xiaomi::adapter) tasks: TaskSet,
    pub(in crate::xiaomi::adapter) subscriptions: Rc<SubscriptionBudget>,
}

// Online and property handshakes share capacity, leaving transport slots for cleanup.
const SUBSCRIPTION_LIMIT: usize = 16;

#[derive(Default)]
pub(in crate::xiaomi::adapter) struct SubscriptionBudget {
    in_flight: Cell<usize>,
    released: Event,
}
pub(in crate::xiaomi::adapter) struct SubscriptionPermit<'a>(&'a SubscriptionBudget);
impl SubscriptionBudget {
    pub(in crate::xiaomi::adapter) async fn acquire(
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
#[derive(Clone, Eq, PartialEq)]
pub(super) struct ConnectionKey {
    pub(super) uid: String,
    pub(super) token: String,
    pub(super) client: String,
    pub(super) home: String,
}
pub(super) fn connection_key(context: &Context) -> Option<ConnectionKey> {
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

fn online_work(context: &Context, did: &str) -> Option<Rc<DeviceWork>> {
    let live = context.live.borrow();
    let slot = live.devices.get(did)?;
    if context.is_virtual_group(did) {
        return None;
    }
    let work = slot.admitted.as_ref()?;
    if work.life.is_closed() || slot.cloud_forbidden || slot.cloud_access.is_none() {
        return None;
    }
    Some(work.clone())
}
pub(super) fn reconcile_online(context: &Rc<Context>, session: Rc<CloudSession>) {
    let works = context
        .live
        .borrow()
        .devices
        .values()
        .filter_map(|slot| slot.admitted.clone())
        .collect::<Vec<_>>();
    for work in works {
        let did = &work.key.id;
        if online_work(context, did).is_none() {
            work.retire_online();
            continue;
        }
        let Some(state) = work.begin_online(&session.handle) else {
            continue;
        };
        session.tasks.spawn(prepare_online(
            context.clone(),
            work,
            session.handle.clone(),
            session.key.token.clone(),
            session.subscriptions.clone(),
            state,
        ));
    }
}

async fn prepare_online(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
    subscriptions: Rc<SubscriptionBudget>,
    state: Rc<OnlinePreparation>,
) -> Result<(), AdapterError> {
    let did = &work.key.id;
    let current = || {
        context.current_notifications(&handle)
            && online_work(&context, did).is_some_and(|current| Rc::ptr_eq(&current, &work))
            && work.online_current(&handle, &state)
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
        let result = handle.subscribe_online(did).await;
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
    work.online_failed(&handle, &state);
    Ok(())
}
pub(super) async fn prepare_properties(
    context: Rc<Context>,
    work: Rc<DeviceWork>,
    handle: NotificationHandle,
    token: String,
    subscriptions: Rc<SubscriptionBudget>,
    preparation: u64,
) -> Result<(), AdapterError> {
    let did = work.key.id.clone();
    let current = || {
        work.cloud_current(&handle, preparation)
            && context.current_notifications(&handle)
            && context.cloud_observation_candidate(&did, false).is_ok()
    };
    let Some(mut pending) = work.cloud_pending(&handle, preparation) else {
        work.cloud_abandoned(&handle, preparation);
        return Ok(());
    };
    'attempts: for attempt in 0..4 {
        let mut failed = Vec::new();
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
            let receipt_work = work.clone();
            let receipt_context = context.clone();
            let receipt_handle = handle.clone();
            let on_result = move |result: Result<(), NotificationError>| {
                if result.is_ok()
                    && receipt_context.current_notifications(&receipt_handle)
                    && receipt_work.cloud_subscribed(&receipt_handle, preparation, binding)
                {
                    receipt_context.changed.notify(usize::MAX);
                }
            };
            let result = match binding {
                SubscriptionBinding::Property(binding) => {
                    handle
                        .subscribe_property_with_result(&did, binding, on_result)
                        .await
                }
                SubscriptionBinding::Event(binding) => {
                    handle
                        .subscribe_event_with_result(&did, binding, on_result)
                        .await
                }
            };
            drop(permit);
            if !current() {
                break 'attempts;
            }
            match result {
                Ok(()) => {}
                Err(NotificationError::Unauthorized) => {
                    context.token_rejected(
                        &token,
                        FailureReason::CloudNotification(NotificationError::Unauthorized),
                    )?;
                    break 'attempts;
                }
                Err(error) => {
                    let message = format!("{did} cloud subscribe {binding}: {error}");
                    work.subscription_failed(
                        &NotificationSource::Cloud(handle.clone()),
                        preparation,
                        binding,
                        message,
                    );
                    failed.push(binding);
                }
            }
        }
        if failed.is_empty() || attempt == 3 {
            work.cloud_finished(&handle, preparation);
            work.report(&context, &did);
            break;
        }
        pending = failed;
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_secs(1))
            .await;
    }
    work.cloud_abandoned(&handle, preparation);
    context.changed.notify(usize::MAX);
    Ok(())
}
#[cfg(test)]
use super::tests::support::connect;
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
pub(super) async fn connections(context: Rc<Context>) -> Result<(), AdapterError> {
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
                    subscriptions: Rc::new(SubscriptionBudget::default()),
                });
                *context.notifications.borrow_mut() = Some(session.clone());
                for work in context
                    .live
                    .borrow()
                    .devices
                    .values()
                    .filter_map(|slot| slot.admitted.clone())
                {
                    work.attach_cloud(handle.clone());
                    work.retry_subscriptions();
                    // State inputs recover with their independent read route and
                    // notification baseline; other feedback keeps this connection demand.
                    if work.observer().is_none() && !context.is_virtual_group(&work.key.id) {
                        work.refresh_connection();
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
                            NotificationEvent::EventOccurred {
                                did,
                                binding,
                                arguments: _arguments,
                                source_time,
                            } => events.native_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                source_time,
                            ),
                            NotificationEvent::Online { did, online } => {
                                events.online_event(&event_handle, &did, online)
                            }
                            NotificationEvent::PropertyValue {
                                did,
                                binding,
                                value,
                                update_time,
                            } => events.property_value_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                &value,
                                update_time,
                                false,
                            ),
                            NotificationEvent::PropertySnapshot {
                                did,
                                binding,
                                value,
                                update_time,
                            } => events.property_value_event(
                                &NotificationSource::Cloud(event_handle.clone()),
                                &did,
                                binding,
                                &value,
                                update_time,
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
                    .filter_map(|slot| slot.admitted.clone())
                {
                    work.detach_cloud(&handle);
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

#[cfg(test)]
mod tests;
