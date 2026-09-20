use async_io::Timer;
use event_listener::Event;
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture, future::select_all};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    time::{Duration, Instant},
};

use super::connection::{advance_retry, startup_failure_code};
use super::{
    CLOUD_NOTIFICATION_RETRY_INTERVAL, LOCAL_SESSION_TIMEOUT, XiaomiFailureStage,
    XiaomiSafeFailureCode,
};
use crate::xiaomi::{
    cloud::{CLOUD_MQTT_HOST, CLOUD_MQTT_PORT, CloudNotification},
    mqtt::CloudTlsConfig,
    runtime::{
        CloudNotificationStartup, SessionAuthority, TransportStartupError,
        start_cloud_notifications,
    },
};

#[derive(Clone)]
pub(super) struct CloudConnectionControl {
    desired: Rc<RefCell<BTreeSet<String>>>,
    stopped: Rc<Cell<bool>>,
    changed: Rc<Event>,
}

pub(super) enum CloudConnectionFact {
    Notification(CloudNotification),
    Selection(CloudSelectionResult),
    Disconnected,
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

pub(super) struct CloudConnectionConfig {
    pub(super) oauth_client_uuid: String,
    pub(super) access_token: String,
}

pub(super) type CloudConnectionFactResult = (
    flume::Receiver<CloudConnectionFact>,
    Result<CloudConnectionFact, flume::RecvError>,
);

pub(super) struct CloudSelectionResult {
    pub(super) desired: BTreeSet<String>,
    pub(super) result: Result<u64, crate::xiaomi::mqtt::MqttError>,
}

impl CloudConnectionControl {
    pub(super) fn wants(&self, selected: &BTreeSet<String>) -> bool {
        *self.desired.borrow() == *selected
    }

    pub(super) fn new(desired: BTreeSet<String>) -> Self {
        Self {
            desired: Rc::new(RefCell::new(desired)),
            stopped: Rc::new(Cell::new(false)),
            changed: Rc::new(Event::new()),
        }
    }

    pub(super) fn set_desired(&self, desired: BTreeSet<String>) {
        if *self.desired.borrow() != desired {
            *self.desired.borrow_mut() = desired;
            self.changed.notify(usize::MAX);
        }
    }

    pub(super) fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }
}

pub(super) fn cloud_connection_fact(
    receiver: flume::Receiver<CloudConnectionFact>,
) -> LocalBoxFuture<'static, CloudConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

pub(super) fn active_cloud_connection_lifetime(
    handle: crate::xiaomi::cloud::CloudNotificationHandle,
    authority: SessionAuthority,
    notifications: flume::Receiver<CloudNotification>,
    mut session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    mut selected: BTreeSet<String>,
    control: CloudConnectionControl,
    facts: flume::Sender<CloudConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut observed_desired = selected.clone();
        let mut next_operation = Instant::now();
        let mut operation = None::<LocalBoxFuture<'static, CloudSelectionResult>>;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired != observed_desired {
                observed_desired = desired.clone();
                next_operation = Instant::now();
            }
            if operation.is_none() && desired != selected && Instant::now() >= next_operation {
                let operation_handle = handle.clone();
                let values = desired.iter().cloned().collect();
                let guard = authority.mqtt_guard();
                let result_desired = desired.clone();
                operation = Some(
                    async move {
                        CloudSelectionResult {
                            desired: result_desired,
                            result: operation_handle
                                .select_dids_guarded(
                                    values,
                                    Instant::now() + LOCAL_SESSION_TIMEOUT,
                                    guard,
                                )
                                .await,
                        }
                    }
                    .boxed_local(),
                );
            }
            enum Ready {
                Stopped,
                Changed,
                Notification(Result<CloudNotification, flume::RecvError>),
                Selection(CloudSelectionResult),
                Retry,
            }
            let listener = control.changed.listen();
            let mut waits = vec![
                session.as_mut().map(|_| Ready::Stopped).boxed_local(),
                async {
                    listener.await;
                    Ready::Changed
                }
                .boxed_local(),
                async { Ready::Notification(notifications.recv_async().await) }.boxed_local(),
            ];
            if let Some(current) = operation.as_mut() {
                waits.push(current.as_mut().map(Ready::Selection).boxed_local());
            } else if desired != selected && Instant::now() < next_operation {
                waits.push(
                    async {
                        Timer::at(next_operation).await;
                        Ready::Retry
                    }
                    .boxed_local(),
                );
            }
            let ready = {
                let (ready, _, _) = select_all(waits).await;
                ready
            };
            match ready {
                Ready::Stopped | Ready::Notification(Err(_)) => return,
                Ready::Changed | Ready::Retry => {}
                Ready::Notification(Ok(notification)) => {
                    if facts
                        .try_send(CloudConnectionFact::Notification(notification))
                        .is_err()
                    {
                        return;
                    }
                }
                Ready::Selection(completed) => {
                    operation = None;
                    if completed.result.is_ok() {
                        selected = completed.desired.clone();
                        next_operation = Instant::now();
                    } else {
                        next_operation = Instant::now() + CLOUD_NOTIFICATION_RETRY_INTERVAL;
                    }
                    if facts
                        .try_send(CloudConnectionFact::Selection(completed))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
    .boxed_local()
}

pub(super) fn cloud_connection_lifetime(
    config: CloudConnectionConfig,
    authority: SessionAuthority,
    control: CloudConnectionControl,
    facts: flume::Sender<CloudConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut retry_delay = CLOUD_NOTIFICATION_RETRY_INTERVAL;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired.is_empty() {
                control.changed.listen().await;
                continue;
            }
            let Ok(tls) = CloudTlsConfig::webpki(CLOUD_MQTT_HOST) else {
                return;
            };
            let deadline = Instant::now() + LOCAL_SESSION_TIMEOUT;
            let startup = CloudNotificationStartup {
                host: CLOUD_MQTT_HOST.into(),
                port: CLOUD_MQTT_PORT,
                tls,
                oauth_client_uuid: config.oauth_client_uuid.clone(),
                access_token: config.access_token.clone(),
                keep_alive: Duration::from_secs(60),
                dids: desired.iter().cloned().collect(),
                deadline,
            };
            enum Startup {
                Started(
                    Result<
                        crate::xiaomi::runtime::RunningCloudNotifications,
                        TransportStartupError,
                    >,
                ),
                Changed,
            }
            let started = {
                let startup = start_cloud_notifications(startup);
                futures_lite::pin!(startup);
                loop {
                    let listener = control.changed.listen();
                    if control.stopped.get() {
                        break Startup::Changed;
                    }
                    match future::or(startup.as_mut().map(Startup::Started), async {
                        listener.await;
                        Startup::Changed
                    })
                    .await
                    {
                        Startup::Changed if !control.stopped.get() => continue,
                        value => break value,
                    }
                }
            };
            let running = match started {
                Startup::Changed => return,
                Startup::Started(Ok(running)) => running,
                Startup::Started(Err(error)) => {
                    let _ = facts.try_send(CloudConnectionFact::Failure {
                        stage: XiaomiFailureStage::Connect,
                        code: startup_failure_code(&error),
                    });
                    let delay = advance_retry(&mut retry_delay, CLOUD_NOTIFICATION_RETRY_INTERVAL);
                    let listener = control.changed.listen();
                    future::or(
                        async {
                            Timer::after(delay).await;
                        },
                        async {
                            listener.await;
                        },
                    )
                    .await;
                    continue;
                }
            };
            retry_delay = CLOUD_NOTIFICATION_RETRY_INTERVAL;
            let (handle, notifications, generation, task) = running.into_parts();
            if facts
                .try_send(CloudConnectionFact::Selection(CloudSelectionResult {
                    desired: desired.clone(),
                    result: Ok(generation),
                }))
                .is_err()
            {
                return;
            }
            active_cloud_connection_lifetime(
                handle,
                authority.clone(),
                notifications,
                task,
                desired,
                control.clone(),
                facts.clone(),
            )
            .await;
            if control.stopped.get() {
                return;
            }
            if facts.try_send(CloudConnectionFact::Disconnected).is_err() {
                return;
            }
            let delay = advance_retry(&mut retry_delay, CLOUD_NOTIFICATION_RETRY_INTERVAL);
            let listener = control.changed.listen();
            future::or(
                async {
                    Timer::after(delay).await;
                },
                async {
                    listener.await;
                },
            )
            .await;
        }
    }
    .boxed_local()
}
