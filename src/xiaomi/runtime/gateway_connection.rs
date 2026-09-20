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

use super::connection::{ConnectionSetup, advance_retry, startup_failure_code};
use super::{
    AUTH_OBSERVE_INTERVAL, LOCAL_RETRY_INTERVAL, LOCAL_SESSION_TIMEOUT, XiaomiFailureStage,
    XiaomiSafeFailureCode,
};
use crate::xiaomi::{
    certificate::XIAOMI_CA_PEM,
    discovery::{GatewayCandidate, GatewayEndpoint, NetworkUpdate},
    gateway::GatewayNotification,
    mqtt::{GatewayTlsConfig, MqttConfig},
    runtime::{GatewayStartup, SessionAuthority, TransportStartupError, start_gateway},
};

#[derive(Clone)]
pub(super) struct GatewayReady {
    pub(super) attempt: u64,
    pub(super) endpoint: GatewayEndpoint,
    pub(super) handle: crate::xiaomi::gateway::GatewayHandle,
    pub(super) evidence: crate::xiaomi::gateway::GatewayEvidence,
    pub(super) authority: SessionAuthority,
}

pub(super) struct GatewayConnectionConfig {
    pub(super) candidate: GatewayCandidate,
    pub(super) endpoints: Vec<(GatewayEndpoint, crate::xiaomi::discovery::NetworkInterface)>,
    pub(super) network: NetworkUpdate,
    pub(super) virtual_did: String,
    pub(super) private_key_pem: String,
    pub(super) certificate_pem: String,
    pub(super) authority: SessionAuthority,
    pub(super) setup: ConnectionSetup,
    #[cfg(test)]
    pub(super) startup: RefCell<
        Option<
            LocalBoxFuture<
                'static,
                Result<crate::xiaomi::runtime::RunningGateway, TransportStartupError>,
            >,
        >,
    >,
}

struct GatewayAttemptScope(SessionAuthority);

impl Drop for GatewayAttemptScope {
    fn drop(&mut self) {
        self.0.revoke();
    }
}

#[derive(Clone)]
pub(super) struct GatewayConnectionControl {
    desired: Rc<RefCell<BTreeSet<String>>>,
    refresh: Rc<Cell<u64>>,
    stopped: Rc<Cell<bool>>,
    publication: Rc<Cell<GatewayPublication>>,
    changed: Rc<Event>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum GatewayPublication {
    Pending,
    Active,
    Rejected,
}

pub(super) enum GatewayConnectionFact {
    Ready(Box<GatewayReady>),
    Disconnected {
        attempt: u64,
    },
    Notification(GatewayNotification),
    Operation(GatewayOperationResult),
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

pub(super) type GatewayConnectionFactResult = (
    flume::Receiver<GatewayConnectionFact>,
    Result<GatewayConnectionFact, flume::RecvError>,
);

pub(super) enum GatewayOperationResult {
    Selected {
        did: u64,
        desired: BTreeSet<String>,
        result: Result<u64, crate::xiaomi::gateway::GatewayError>,
    },
    Refreshed {
        did: u64,
        result:
            Result<crate::xiaomi::gateway::GatewayEvidence, crate::xiaomi::gateway::GatewayError>,
    },
}

impl GatewayConnectionControl {
    #[cfg(test)]
    pub(super) fn desired_is_empty(&self) -> bool {
        self.desired.borrow().is_empty()
    }

    pub(super) fn new() -> Self {
        Self {
            desired: Rc::new(RefCell::new(BTreeSet::new())),
            refresh: Rc::new(Cell::new(0)),
            stopped: Rc::new(Cell::new(false)),
            publication: Rc::new(Cell::new(GatewayPublication::Pending)),
            changed: Rc::new(Event::new()),
        }
    }

    pub(super) fn set_desired(&self, desired: BTreeSet<String>) {
        if *self.desired.borrow() != desired {
            *self.desired.borrow_mut() = desired;
            self.changed.notify(usize::MAX);
        }
    }

    pub(super) fn refresh(&self) {
        self.refresh.set(self.refresh.get().wrapping_add(1));
        self.changed.notify(usize::MAX);
    }

    pub(super) fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }

    pub(super) fn publish(&self, publication: GatewayPublication) {
        self.publication.set(publication);
        self.changed.notify(usize::MAX);
    }
}

pub(super) fn gateway_connection_fact(
    receiver: flume::Receiver<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, GatewayConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

pub(super) fn active_gateway_connection_lifetime(
    did: u64,
    handle: crate::xiaomi::gateway::GatewayHandle,
    authority: SessionAuthority,
    notifications: flume::Receiver<GatewayNotification>,
    mut session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    control: GatewayConnectionControl,
    facts: flume::Sender<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut selected = BTreeSet::new();
        let mut completed_refresh = control.refresh.get();
        let mut observed_desired = control.desired.borrow().clone();
        let mut next_operation = Instant::now();
        let mut operation = None::<LocalBoxFuture<'static, GatewayOperationResult>>;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired != observed_desired {
                observed_desired = desired.clone();
                next_operation = Instant::now();
            }
            if operation.is_none() {
                if desired != selected && Instant::now() >= next_operation {
                    let values = desired.iter().cloned().collect();
                    let operation_handle = handle.clone();
                    let result_desired = desired.clone();
                    operation = Some(
                        async move {
                            let result = operation_handle
                                .select_notifications(
                                    values,
                                    Instant::now() + LOCAL_SESSION_TIMEOUT,
                                )
                                .await;
                            GatewayOperationResult::Selected {
                                did,
                                desired: result_desired,
                                result,
                            }
                        }
                        .boxed_local(),
                    );
                } else if desired == selected
                    && completed_refresh != control.refresh.get()
                    && Instant::now() >= next_operation
                {
                    let operation_handle = handle.clone();
                    let guard = authority.mqtt_guard();
                    operation = Some(
                        async move {
                            GatewayOperationResult::Refreshed {
                                did,
                                result: operation_handle
                                    .get_devices(Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                                    .await,
                            }
                        }
                        .boxed_local(),
                    );
                }
            }
            enum Ready {
                Stopped,
                Changed,
                Notification(Result<GatewayNotification, flume::RecvError>),
                Operation(GatewayOperationResult),
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
                waits.push(current.as_mut().map(Ready::Operation).boxed_local());
            } else if (desired != selected || completed_refresh != control.refresh.get())
                && Instant::now() < next_operation
            {
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
                        .try_send(GatewayConnectionFact::Notification(notification))
                        .is_err()
                    {
                        return;
                    }
                }
                Ready::Operation(result) => {
                    operation = None;
                    match &result {
                        GatewayOperationResult::Selected {
                            desired, result, ..
                        } if result.is_ok() => {
                            selected = desired.clone();
                            next_operation = Instant::now();
                        }
                        GatewayOperationResult::Refreshed { result, .. } if result.is_ok() => {
                            completed_refresh = control.refresh.get();
                            next_operation = Instant::now();
                        }
                        _ => {
                            next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
                        }
                    }
                    if facts
                        .try_send(GatewayConnectionFact::Operation(result))
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

pub(super) fn gateway_full_connection_lifetime(
    config: GatewayConnectionConfig,
    control: GatewayConnectionControl,
    facts: flume::Sender<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut endpoint_index = 0;
        let mut next_attempt = 1_u64;
        let mut retry_delay = LOCAL_RETRY_INTERVAL;
        loop {
            if control.stopped.get() {
                return;
            }
            let (endpoint, _interface) =
                config.endpoints[endpoint_index % config.endpoints.len()].clone();
            endpoint_index = endpoint_index.wrapping_add(1);
            let attempt = next_attempt;
            next_attempt = next_attempt.wrapping_add(1).max(1);
            let attempt_authority = config.authority.fresh_lease();
            let _attempt_scope = GatewayAttemptScope(attempt_authority.clone());
            let Some(setup_slot) = config
                .setup
                .acquire(&control.stopped, &control.changed)
                .await
            else {
                return;
            };
            #[cfg(test)]
            let injected = config.startup.borrow_mut().take();
            #[cfg(test)]
            let startup = if let Some(startup) = injected {
                startup
            } else {
                let Ok(tls) = GatewayTlsConfig::new(
                    XIAOMI_CA_PEM,
                    &config.certificate_pem,
                    &config.private_key_pem,
                ) else {
                    return;
                };
                start_gateway(GatewayStartup {
                    endpoint: std::net::SocketAddrV4::new(endpoint.address, endpoint.port),
                    tls,
                    mqtt: MqttConfig::new(
                        config.virtual_did.clone(),
                        None,
                        Duration::from_secs(60),
                    ),
                    virtual_did: config.virtual_did.clone(),
                    gateway_did: config.candidate.gateway_did,
                    peer_did: config.candidate.gateway_did.to_string(),
                    epoch: config.network.epoch,
                    deadline: Instant::now() + LOCAL_SESSION_TIMEOUT,
                    guard: attempt_authority.mqtt_guard(),
                })
                .boxed_local()
            };
            #[cfg(not(test))]
            let startup = {
                let Ok(tls) = GatewayTlsConfig::new(
                    XIAOMI_CA_PEM,
                    &config.certificate_pem,
                    &config.private_key_pem,
                ) else {
                    return;
                };
                start_gateway(GatewayStartup {
                    endpoint: std::net::SocketAddrV4::new(endpoint.address, endpoint.port),
                    tls,
                    mqtt: MqttConfig::new(
                        config.virtual_did.clone(),
                        None,
                        Duration::from_secs(60),
                    ),
                    virtual_did: config.virtual_did.clone(),
                    gateway_did: config.candidate.gateway_did,
                    peer_did: config.candidate.gateway_did.to_string(),
                    epoch: config.network.epoch,
                    deadline: Instant::now() + LOCAL_SESSION_TIMEOUT,
                    guard: attempt_authority.mqtt_guard(),
                })
                .boxed_local()
            };
            enum Startup {
                Started(Result<crate::xiaomi::runtime::RunningGateway, TransportStartupError>),
                Changed,
            }
            let started = {
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
            drop(setup_slot);
            let running = match started {
                Startup::Changed => return,
                Startup::Started(Ok(running)) => running,
                Startup::Started(Err(error)) => {
                    let _ = facts.try_send(GatewayConnectionFact::Failure {
                        stage: XiaomiFailureStage::Connect,
                        code: startup_failure_code(&error),
                    });
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
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
            retry_delay = LOCAL_RETRY_INTERVAL;
            let parts = running.into_parts();
            let ready = GatewayReady {
                attempt,
                endpoint: endpoint.clone(),
                handle: parts.handle.clone(),
                evidence: parts.evidence,
                authority: attempt_authority.clone(),
            };
            let mut session = parts.task;
            while control.publication.get() == GatewayPublication::Pending {
                let _ = facts.try_send(GatewayConnectionFact::Ready(Box::new(ready.clone())));
                let listener = control.changed.listen();
                enum Pending {
                    Session,
                    Changed,
                    Retry,
                }
                let (event, _, _) = select_all(vec![
                    session.as_mut().map(|_| Pending::Session).boxed_local(),
                    async {
                        listener.await;
                        Pending::Changed
                    }
                    .boxed_local(),
                    async {
                        Timer::after(AUTH_OBSERVE_INTERVAL).await;
                        Pending::Retry
                    }
                    .boxed_local(),
                ])
                .await;
                if matches!(event, Pending::Session) {
                    break;
                }
                if control.stopped.get() {
                    return;
                }
            }
            match control.publication.get() {
                GatewayPublication::Rejected => return,
                GatewayPublication::Pending => {
                    attempt_authority.revoke();
                    if facts
                        .send_async(GatewayConnectionFact::Disconnected { attempt })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
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
                GatewayPublication::Active => {}
            }
            active_gateway_connection_lifetime(
                config.candidate.gateway_did,
                parts.handle,
                attempt_authority.clone(),
                parts.notifications,
                session,
                control.clone(),
                facts.clone(),
            )
            .await;
            attempt_authority.revoke();
            if control.stopped.get() {
                return;
            }
            control.publication.set(GatewayPublication::Pending);
            if facts
                .send_async(GatewayConnectionFact::Disconnected { attempt })
                .await
                .is_err()
            {
                return;
            }
            let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
            Timer::after(delay).await;
        }
    }
    .boxed_local()
}
