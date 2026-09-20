use async_io::Timer;
use event_listener::Event;
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture, future::select_all};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use super::connection::{ConnectionSetup, advance_retry, lan_failure_code};
use super::{
    AUTH_OBSERVE_INTERVAL, LAN_SUBSCRIPTION_RENEWAL, LOCAL_RETRY_INTERVAL, LOCAL_SESSION_TIMEOUT,
    XiaomiFailureStage, XiaomiSafeFailureCode, unix_time,
};
use crate::{
    device::PhysicalDeviceId,
    xiaomi::{
        discovery::NetworkUpdate,
        lan::{LanEventArguments, LanNotification, LanProperty, LanTarget},
        runtime::{
            CurrentSessionRegistry, LanOperationEvidence, PushSource, SessionAuthority,
            StateRuntime, SubscriptionToken, start_lan,
        },
    },
};

pub(super) struct LanConnectionConfig {
    pub(super) physical: PhysicalDeviceId,
    pub(super) targets: Vec<LanTarget>,
    pub(super) network: NetworkUpdate,
    pub(super) account: crate::device::AccountId,
    pub(super) session_generation: crate::storage::AuthSessionGeneration,
    pub(super) descriptor: crate::xiaomi::catalog::FeatureDescriptor,
    pub(super) property: LanProperty,
    pub(super) virtual_did: u64,
    pub(super) setup: ConnectionSetup,
    #[cfg(test)]
    pub(super) startup: RefCell<
        Option<
            LocalBoxFuture<
                'static,
                Result<crate::xiaomi::runtime::RunningLan, crate::xiaomi::lan::LanError>,
            >,
        >,
    >,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum LanPublication {
    Pending,
    Active,
    Rejected,
}

#[derive(Clone)]
pub(super) struct LanConnectionControl {
    stopped: Rc<Cell<bool>>,
    reconnect: Rc<Cell<u64>>,
    desired_push: Rc<Cell<bool>>,
    push_active: Rc<Cell<bool>>,
    publication: Rc<Cell<LanPublication>>,
    changed: Rc<Event>,
    runtime_wake: Rc<Event>,
    #[cfg(test)]
    pub(super) ready_seen: Rc<Cell<u64>>,
    #[cfg(test)]
    pub(super) ready_stage: Rc<Cell<u8>>,
}

#[derive(Clone)]
pub(super) struct LanReady {
    pub(super) generation: u64,
    pub(super) attempt: u64,
    pub(super) target: LanTarget,
    pub(super) network: NetworkUpdate,
    pub(super) account: crate::device::AccountId,
    pub(super) session_generation: crate::storage::AuthSessionGeneration,
    pub(super) descriptor: crate::xiaomi::catalog::FeatureDescriptor,
    pub(super) evidence: crate::xiaomi::lan::LanEvidence,
    pub(super) operation: LanOperationEvidence,
    pub(super) resources: Rc<LanAttemptResources>,
}

pub(super) enum LanConnectionFact {
    Ready(Box<LanReady>),
    Disconnected {
        generation: u64,
        attempt: u64,
    },
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

pub(super) type LanConnectionFactResult = (
    flume::Receiver<LanConnectionFact>,
    Result<LanConnectionFact, flume::RecvError>,
);

pub(super) struct LanAttemptResources {
    device: PhysicalDeviceId,
    handle: crate::xiaomi::lan::LanHandle,
    authority: SessionAuthority,
    registry: CurrentSessionRegistry,
    state: StateRuntime,
    route: RefCell<Option<crate::xiaomi::runtime::RouteLease>>,
    token: RefCell<Option<SubscriptionToken>>,
    closed: Cell<bool>,
}

struct LanAttemptScope(Rc<LanAttemptResources>);

impl LanConnectionControl {
    pub(super) fn generation(&self) -> u64 {
        self.reconnect.get()
    }
    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.get()
    }
    pub(super) fn push_active(&self) -> bool {
        self.push_active.get()
    }
    #[cfg(test)]
    pub(super) fn publication(&self) -> LanPublication {
        self.publication.get()
    }

    pub(super) fn new(runtime_wake: Rc<Event>) -> Self {
        Self {
            stopped: Rc::new(Cell::new(false)),
            reconnect: Rc::new(Cell::new(1)),
            desired_push: Rc::new(Cell::new(false)),
            push_active: Rc::new(Cell::new(false)),
            publication: Rc::new(Cell::new(LanPublication::Pending)),
            changed: Rc::new(Event::new()),
            runtime_wake,
            #[cfg(test)]
            ready_seen: Rc::new(Cell::new(0)),
            #[cfg(test)]
            ready_stage: Rc::new(Cell::new(0)),
        }
    }

    pub(super) fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }

    pub(super) fn reconnect(&self) {
        self.publication.set(LanPublication::Pending);
        self.reconnect
            .set(self.reconnect.get().wrapping_add(1).max(1));
        self.changed.notify(usize::MAX);
    }

    pub(super) fn activate(&self) {
        self.publication.set(LanPublication::Active);
        self.changed.notify(usize::MAX);
    }

    pub(super) fn reject(&self) {
        self.publication.set(LanPublication::Rejected);
        self.changed.notify(usize::MAX);
    }

    pub(super) fn set_desired_push(&self, selected: bool) {
        if self.desired_push.replace(selected) != selected {
            self.changed.notify(usize::MAX);
        }
    }

    pub(super) fn set_push_active(&self, active: bool) {
        if self.push_active.replace(active) != active {
            self.runtime_wake.notify(usize::MAX);
        }
    }
}

impl LanAttemptResources {
    pub(super) fn new(
        device: PhysicalDeviceId,
        handle: crate::xiaomi::lan::LanHandle,
        authority: SessionAuthority,
        registry: CurrentSessionRegistry,
        state: StateRuntime,
    ) -> Self {
        Self {
            device,
            handle,
            authority,
            registry,
            state,
            route: RefCell::new(None),
            token: RefCell::new(None),
            closed: Cell::new(false),
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub(super) fn install_route(
        &self,
        descriptor: Option<crate::xiaomi::catalog::FeatureDescriptor>,
    ) {
        let lease = self.registry.install_lan(
            self.device.clone(),
            self.handle.clone(),
            self.authority.clone(),
            descriptor,
        );
        self.route.replace(Some(lease));
    }

    pub(super) fn replace_token(&self, token: Option<SubscriptionToken>) {
        if let Some(previous) = self.token.replace(token) {
            self.state.source_failed(&previous);
        }
    }

    pub(super) fn token(&self) -> Option<SubscriptionToken> {
        self.token.borrow().clone()
    }

    pub(super) fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.authority.revoke();
        self.handle.stop();
        if let Some(token) = self.token.borrow_mut().take() {
            self.state.source_failed(&token);
        }
        if let Some(lease) = self.route.borrow_mut().take() {
            self.registry.revoke_lan_if_authority(&self.device, &lease);
        }
    }
}

impl Drop for LanAttemptResources {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for LanAttemptScope {
    fn drop(&mut self) {
        self.0.close();
    }
}

pub(super) fn lan_connection_fact(
    receiver: flume::Receiver<LanConnectionFact>,
) -> LocalBoxFuture<'static, LanConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

pub(super) fn lan_connection_lifetime(
    config: LanConnectionConfig,
    control: LanConnectionControl,
    facts: flume::Sender<LanConnectionFact>,
    registry: CurrentSessionRegistry,
    state: StateRuntime,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut target_index = 0_usize;
        let mut next_attempt = 1_u64;
        let mut retry_delay = LOCAL_RETRY_INTERVAL;
        while !control.stopped.get() {
            let target = config.targets[target_index % config.targets.len()].clone();
            target_index = target_index.wrapping_add(1);
            let authority = SessionAuthority::new();
            let Some(setup_slot) = config
                .setup
                .acquire(&control.stopped, &control.changed)
                .await
            else {
                return;
            };
            #[cfg(test)]
            let injected = config.startup.borrow_mut().take();
            #[cfg(not(test))]
            let injected: Option<
                LocalBoxFuture<
                    'static,
                    Result<crate::xiaomi::runtime::RunningLan, crate::xiaomi::lan::LanError>,
                >,
            > = None;
            let running = match injected {
                Some(startup) => startup.await,
                None => {
                    start_lan(
                        target.clone(),
                        config.virtual_did,
                        config.property,
                        Instant::now() + LOCAL_SESSION_TIMEOUT,
                        authority.lan_guard(),
                    )
                    .await
                }
            };
            drop(setup_slot);
            let running = match running {
                Ok(running) => running,
                Err(error) => {
                    let _ = facts.try_send(LanConnectionFact::Failure {
                        stage: XiaomiFailureStage::Authenticate,
                        code: lan_failure_code(error.kind()),
                    });
                    control.runtime_wake.notify(usize::MAX);
                    authority.revoke();
                    if !target_index.is_multiple_of(config.targets.len()) {
                        continue;
                    }
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
                    if !wait_for_lan_retry(&control, delay).await {
                        return;
                    }
                    continue;
                }
            };
            retry_delay = LOCAL_RETRY_INTERVAL;
            let operation = running.operation;
            let evidence = running.evidence.clone();
            let (handle, notifications, _, _, mut session) = running.into_parts();
            let resources = Rc::new(LanAttemptResources::new(
                config.physical.clone(),
                handle,
                authority,
                registry.clone(),
                state.clone(),
            ));
            let _attempt_scope = LanAttemptScope(resources.clone());
            let generation = control.reconnect.get();
            let attempt = next_attempt;
            next_attempt = next_attempt.wrapping_add(1).max(1);
            let ready = LanReady {
                generation,
                attempt,
                target,
                network: config.network.clone(),
                account: config.account.clone(),
                session_generation: config.session_generation,
                descriptor: config.descriptor.clone(),
                evidence,
                operation,
                resources: resources.clone(),
            };
            let mut connected = true;
            while control.publication.get() == LanPublication::Pending
                && !control.stopped.get()
                && generation == control.reconnect.get()
            {
                if facts.is_empty() {
                    match facts.try_send(LanConnectionFact::Ready(Box::new(ready.clone()))) {
                        Ok(()) | Err(flume::TrySendError::Full(_)) => {}
                        Err(flume::TrySendError::Disconnected(_)) => return,
                    }
                }
                control.runtime_wake.notify(usize::MAX);
                enum PublicationWait {
                    Changed,
                    Retry,
                    Stopped,
                }
                let listener = control.changed.listen();
                let event = future::or(
                    async {
                        let _ = session.as_mut().await;
                        PublicationWait::Stopped
                    },
                    future::or(
                        async {
                            listener.await;
                            PublicationWait::Changed
                        },
                        async {
                            Timer::after(AUTH_OBSERVE_INTERVAL).await;
                            PublicationWait::Retry
                        },
                    ),
                )
                .await;
                if matches!(event, PublicationWait::Stopped) {
                    connected = false;
                    break;
                }
            }
            if connected
                && control.publication.get() == LanPublication::Active
                && generation == control.reconnect.get()
                && !control.stopped.get()
            {
                connected = run_active_lan_connection(
                    &config.physical,
                    generation,
                    &control,
                    &resources,
                    notifications,
                    &mut session,
                    &facts,
                )
                .await;
            }
            resources.close();
            control.set_push_active(false);
            let rejected = control.publication.get() == LanPublication::Rejected;
            control.publication.set(LanPublication::Pending);
            if facts
                .send_async(LanConnectionFact::Disconnected {
                    generation,
                    attempt,
                })
                .await
                .is_err()
            {
                return;
            }
            control.runtime_wake.notify(usize::MAX);
            if control.stopped.get() || rejected {
                return;
            }
            if connected && generation == control.reconnect.get() {
                continue;
            }
            let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
            if !wait_for_lan_retry(&control, delay).await {
                return;
            }
        }
    }
    .boxed_local()
}

async fn wait_for_lan_retry(control: &LanConnectionControl, delay: Duration) -> bool {
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
    !control.stopped.get()
}

async fn run_active_lan_connection(
    device: &PhysicalDeviceId,
    generation: u64,
    control: &LanConnectionControl,
    resources: &Rc<LanAttemptResources>,
    notifications: flume::Receiver<LanNotification>,
    session: &mut LocalBoxFuture<'static, Result<(), crate::xiaomi::lan::LanError>>,
    facts: &flume::Sender<LanConnectionFact>,
) -> bool {
    enum LanWireOperation {
        Subscribe(
            LocalBoxFuture<
                'static,
                Result<crate::xiaomi::lan::LanSubscription, crate::xiaomi::lan::LanError>,
            >,
        ),
        Unsubscribe(LocalBoxFuture<'static, Result<(), crate::xiaomi::lan::LanError>>),
    }
    let mut subscription = None::<crate::xiaomi::lan::LanSubscription>;
    let mut operation = None::<LanWireOperation>;
    let mut next_operation = Instant::now();
    let mut observed_desired = control.desired_push.get();
    loop {
        let listener = control.changed.listen();
        if control.stopped.get() || generation != control.reconnect.get() {
            return false;
        }
        let desired = control.desired_push.get();
        if desired != observed_desired {
            observed_desired = desired;
            next_operation = Instant::now();
        }
        if operation.is_none() && Instant::now() >= next_operation {
            if desired {
                let handle = resources.handle.clone();
                let guard = resources.authority.lan_guard();
                operation = Some(LanWireOperation::Subscribe(
                    async move {
                        handle
                            .subscribe(Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                            .await
                    }
                    .boxed_local(),
                ));
            } else if let Some(current) = subscription.clone() {
                let handle = resources.handle.clone();
                let guard = resources.authority.lan_guard();
                operation = Some(LanWireOperation::Unsubscribe(
                    async move {
                        handle
                            .unsubscribe(&current, Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                            .await
                    }
                    .boxed_local(),
                ));
            }
        }
        enum LanEvent {
            SessionStopped,
            Changed,
            Notification(Result<LanNotification, flume::RecvError>),
            Subscribed(Result<crate::xiaomi::lan::LanSubscription, crate::xiaomi::lan::LanError>),
            Unsubscribed(Result<(), crate::xiaomi::lan::LanError>),
            OperationDue,
        }
        let mut waits = Vec::<LocalBoxFuture<'_, LanEvent>>::new();
        waits.push(
            session
                .as_mut()
                .map(|_| LanEvent::SessionStopped)
                .boxed_local(),
        );
        waits.push(
            async {
                listener.await;
                LanEvent::Changed
            }
            .boxed_local(),
        );
        waits
            .push(async { LanEvent::Notification(notifications.recv_async().await) }.boxed_local());
        match operation.as_mut() {
            Some(LanWireOperation::Subscribe(operation)) => {
                waits.push(operation.as_mut().map(LanEvent::Subscribed).boxed_local());
            }
            Some(LanWireOperation::Unsubscribe(operation)) => {
                waits.push(operation.as_mut().map(LanEvent::Unsubscribed).boxed_local());
            }
            None if (desired || subscription.is_some()) && Instant::now() < next_operation => {
                waits.push(
                    async move {
                        Timer::at(next_operation).await;
                        LanEvent::OperationDue
                    }
                    .boxed_local(),
                );
            }
            None => {}
        }
        let (event, _, _) = select_all(waits).await;
        match event {
            LanEvent::SessionStopped => return false,
            LanEvent::Changed => {}
            LanEvent::OperationDue => {}
            LanEvent::Notification(Err(_)) => return false,
            LanEvent::Notification(Ok(LanNotification::SubscriptionHint { .. })) => {
                next_operation = Instant::now();
            }
            LanEvent::Notification(Ok(notification)) => {
                if let Some(token) = resources.token() {
                    apply_lan_state_notification(&resources.state, &token, notification);
                }
            }
            LanEvent::Subscribed(Ok(current)) => {
                operation = None;
                subscription = Some(current.clone());
                next_operation = if control.desired_push.get() {
                    Instant::now() + LAN_SUBSCRIPTION_RENEWAL
                } else {
                    Instant::now()
                };
                if control.desired_push.get()
                    && let Some(token) = resources.state.select_push_source(
                        device,
                        PushSource::Lan,
                        resources.handle.did(),
                        current.generation,
                    )
                {
                    resources
                        .state
                        .acknowledge(&token, resources.handle.did(), current.generation);
                    resources.replace_token(Some(token));
                    control.set_push_active(true);
                }
            }
            LanEvent::Subscribed(Err(error)) => {
                let _ = facts.try_send(LanConnectionFact::Failure {
                    stage: XiaomiFailureStage::Subscribe,
                    code: lan_failure_code(error.kind()),
                });
                operation = None;
                subscription = None;
                resources.replace_token(None);
                control.set_push_active(false);
                next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
            }
            LanEvent::Unsubscribed(result) => {
                operation = None;
                if result.is_ok() {
                    subscription = None;
                    resources.replace_token(None);
                    control.set_push_active(false);
                    next_operation = Instant::now();
                } else {
                    if let Err(error) = &result {
                        let _ = facts.try_send(LanConnectionFact::Failure {
                            stage: XiaomiFailureStage::Subscribe,
                            code: lan_failure_code(error.kind()),
                        });
                    }
                    next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
                }
            }
        }
    }
}

fn apply_lan_state_notification(
    state: &StateRuntime,
    token: &SubscriptionToken,
    notification: LanNotification,
) {
    match notification {
        LanNotification::Property {
            did,
            siid,
            piid,
            value,
            generation,
            ..
        } => {
            state.apply_property(
                token,
                did,
                generation,
                siid,
                piid,
                Some(&value),
                unix_time(),
                false,
            );
        }
        LanNotification::Event {
            did,
            siid,
            eiid,
            arguments,
            generation,
            ..
        } => match arguments {
            LanEventArguments::Keyed(arguments) => {
                let values = arguments
                    .into_iter()
                    .map(|argument| (argument.piid, argument.value))
                    .collect::<Vec<_>>();
                state.apply_keyed_event(
                    token,
                    did,
                    generation,
                    siid,
                    eiid,
                    &values,
                    unix_time(),
                    false,
                );
            }
            LanEventArguments::Positional(values) => {
                state.apply_positional_event(
                    token,
                    did,
                    generation,
                    siid,
                    eiid,
                    &values,
                    unix_time(),
                    false,
                );
            }
        },
        LanNotification::SubscriptionHint { .. } => {}
    }
}
