use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use async_io::Timer;
use event_listener::Event;
use flume::Sender;
use futures_lite::future;
use futures_util::{FutureExt, StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};

use crate::{
    device::{
        CommandOutcome, DeviceCommand, DeviceCommandSink, DeviceService, FeatureIdentity,
        PhysicalDeviceId, Property,
    },
    storage::AuthSessionGeneration,
    xiaomi::catalog::{FeatureDescriptor, WireOperation},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPath {
    Gateway,
    Lan,
    Cloud,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OperationPaths {
    pub gateway: bool,
    pub lan: bool,
    pub cloud: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeFeature {
    pub identity: FeatureIdentity,
    pub descriptor: FeatureDescriptor,
    pub authority_generation: u64,
    pub auth_session_generation: AuthSessionGeneration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransportCommand {
    pub device: PhysicalDeviceId,
    pub typed: DeviceCommand,
    pub operation: WireOperation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportFailure {
    Unavailable,
    Rejected(i64),
    Ambiguous,
}

pub trait CommandTransport {
    fn available_paths(&self, _device: &PhysicalDeviceId) -> OperationPaths {
        OperationPaths {
            gateway: true,
            lan: true,
            cloud: true,
        }
    }

    fn send(
        &self,
        path: ControlPath,
        command: TransportCommand,
        timeout: Duration,
        guard: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>>;
}

#[derive(Clone)]
pub struct SendGuard {
    shared: SharedSendState,
    check: Rc<dyn Fn() -> SendAuthorization>,
    cancelled: Rc<Event>,
}

impl SendGuard {
    pub fn authorization(&self) -> SendAuthorization {
        if self.shared.is_cancelled() {
            SendAuthorization::Revoked
        } else {
            (self.check)()
        }
    }

    pub fn permitted(&self) -> bool {
        self.authorization() == SendAuthorization::Allowed
    }

    pub fn shared_state(&self) -> SharedSendState {
        self.shared.clone()
    }

    pub fn revoke(&self) {
        self.shared.close();
        self.cancelled.notify(usize::MAX);
    }

    pub fn may_have_been_sent(&self) -> bool {
        self.shared.may_have_been_sent()
    }

    pub async fn cancelled(&self) {
        loop {
            let listener = self.cancelled.listen();
            if self.shared.is_cancelled() {
                if !self.shared.may_have_been_sent() {
                    return;
                }
                futures_lite::future::pending::<()>().await;
            }
            listener.await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendAuthorization {
    Allowed,
    Revoked,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeDiagnostic {
    CommandFailure {
        feature: FeatureIdentity,
        command: DeviceCommand,
        path: ControlPath,
        stage: CommandFailureStage,
        outcome: CommandOutcome,
        sent: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandFailureStage {
    BeforeSend,
    AfterSend,
    DeviceResponse,
}

#[derive(Clone)]
pub struct SharedSendState {
    state: Arc<AtomicU8>,
    deadline: Instant,
}

impl SharedSendState {
    const CLOSED: u8 = 1;
    const SENT: u8 = 2;

    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) & Self::CLOSED != 0 || Instant::now() >= self.deadline
    }

    /// Records that the selected transport accepted the operation for delivery.
    pub fn mark_sent(&self) {
        self.state.fetch_or(Self::SENT, Ordering::AcqRel);
    }

    pub fn may_have_been_sent(&self) -> bool {
        self.state.load(Ordering::Acquire) & Self::SENT != 0
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn close(&self) -> bool {
        self.state.fetch_or(Self::CLOSED, Ordering::AcqRel) & Self::CLOSED == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CommandLimits {
    pub queue_capacity: usize,
    pub total: Duration,
    pub local_attempt: Duration,
    pub cloud_attempt: Duration,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            queue_capacity: 64,
            total: Duration::from_secs(10),
            local_attempt: Duration::from_secs(3),
            cloud_attempt: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
pub struct CommandRuntime {
    inner: Rc<RefCell<RuntimeState>>,
    service: DeviceService,
    transport: Rc<dyn CommandTransport>,
    limits: CommandLimits,
    wake: Rc<Event>,
    running: Rc<Cell<bool>>,
    stopped: Rc<Cell<bool>>,
    execution_gate: DeviceExecutionGate,
    completions: CommandCompletionLog,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandCompletion {
    pub feature: FeatureIdentity,
    pub command: DeviceCommand,
    pub operation: WireOperation,
    pub outcome: CommandOutcome,
    pub authority_generation: u64,
    pub auth_session_generation: AuthSessionGeneration,
    pub synchronize_by: Instant,
}

#[derive(Clone)]
struct CommandCompletionLog {
    inner: Rc<RefCell<CommandCompletionEntries>>,
    event: Rc<Event>,
}

type CommandCompletionEntries = (u64, VecDeque<(u64, CommandCompletion)>);

pub struct CommandCompletionSubscription {
    log: CommandCompletionLog,
    cursor: u64,
    lagged: bool,
}

impl CommandCompletionSubscription {
    pub fn drain(&mut self) -> Vec<CommandCompletion> {
        let log = self.log.inner.borrow();
        if log
            .1
            .front()
            .is_some_and(|(version, _)| *version > self.cursor.saturating_add(1))
        {
            self.cursor = log.0;
            self.lagged = true;
            return Vec::new();
        }
        let completions = log
            .1
            .iter()
            .filter(|(version, _)| *version > self.cursor)
            .map(|(_, completion)| completion.clone())
            .collect();
        self.cursor = log.0;
        completions
    }

    pub fn take_lagged(&mut self) -> bool {
        std::mem::take(&mut self.lagged)
    }

    pub(crate) fn notifier(&self) -> Rc<Event> {
        self.log.event.clone()
    }

    pub async fn changed(&mut self) -> Vec<CommandCompletion> {
        loop {
            let listener = self.log.event.listen();
            let completions = self.drain();
            if !completions.is_empty() || self.lagged {
                return completions;
            }
            listener.await;
        }
    }
}

#[derive(Clone, Default)]
pub struct DeviceExecutionGate {
    busy: Rc<RefCell<HashSet<PhysicalDeviceId>>>,
    event: Rc<Event>,
}

pub struct DeviceExecutionLease {
    gate: DeviceExecutionGate,
    device: PhysicalDeviceId,
}

impl Drop for DeviceExecutionLease {
    fn drop(&mut self) {
        self.gate.busy.borrow_mut().remove(&self.device);
        self.gate.event.notify(usize::MAX);
    }
}

impl DeviceExecutionGate {
    pub async fn acquire(
        &self,
        device: PhysicalDeviceId,
        deadline: Instant,
    ) -> Option<DeviceExecutionLease> {
        loop {
            let listener = self.event.listen();
            if Instant::now() >= deadline {
                return None;
            }
            if self.busy.borrow_mut().insert(device.clone()) {
                return Some(DeviceExecutionLease {
                    gate: self.clone(),
                    device,
                });
            }
            if !future::race(
                async {
                    listener.await;
                    true
                },
                async {
                    Timer::at(deadline).await;
                    false
                },
            )
            .await
            {
                return None;
            }
        }
    }
}

struct RuntimeState {
    next_job: u64,
    next_target: u64,
    queued: usize,
    queues: BTreeMap<PhysicalDeviceId, VecDeque<QueuedBatch>>,
    features: BTreeMap<FeatureIdentity, RuntimeFeature>,
    action_boundaries: BTreeMap<PhysicalDeviceId, u64>,
    property_generations: BTreeMap<(FeatureIdentity, u64, Property), Vec<TargetGeneration>>,
    off_generations: BTreeMap<(FeatureIdentity, u64), Vec<TargetGeneration>>,
    active: HashMap<u64, ActiveSend>,
    reserved: HashMap<u64, ReservedBatch>,
    diagnostics: VecDeque<RuntimeDiagnostic>,
}

struct ActiveSend {
    device: PhysicalDeviceId,
    feature: FeatureIdentity,
    guard: SendGuard,
    cancelled: Rc<Cell<bool>>,
    reply: Sender<CommandOutcome>,
}

struct ReservedBatch {
    device: PhysicalDeviceId,
    feature: FeatureIdentity,
    cancelled: Rc<Cell<bool>>,
    send_state: Rc<RefCell<Option<SharedSendState>>>,
    reply: Sender<CommandOutcome>,
}

struct QueuedBatch {
    id: u64,
    feature: FeatureIdentity,
    steps: Vec<QueuedStep>,
    deadline: Instant,
    cancelled: Rc<Cell<bool>>,
    send_state: Rc<RefCell<Option<SharedSendState>>>,
    reply: Sender<CommandOutcome>,
    authority: QueuedAuthority,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct QueuedAuthority {
    authority_generation: u64,
    auth_session_generation: AuthSessionGeneration,
}

struct QueuedStep {
    command: DeviceCommand,
    property_generation: Option<u64>,
    action_boundary: u64,
    off_generation: u64,
    off_sensitive: bool,
}

struct TargetGeneration {
    generation: u64,
    cancelled: Weak<Cell<bool>>,
}

impl CommandRuntime {
    pub fn new(service: DeviceService, transport: Rc<dyn CommandTransport>) -> Self {
        Self::with_limits(service, transport, CommandLimits::default())
    }

    pub fn with_limits(
        service: DeviceService,
        transport: Rc<dyn CommandTransport>,
        limits: CommandLimits,
    ) -> Self {
        let runtime = Self {
            inner: Rc::new(RefCell::new(RuntimeState {
                next_job: 1,
                next_target: 1,
                queued: 0,
                queues: BTreeMap::new(),
                features: BTreeMap::new(),
                action_boundaries: BTreeMap::new(),
                property_generations: BTreeMap::new(),
                off_generations: BTreeMap::new(),
                active: HashMap::new(),
                reserved: HashMap::new(),
                diagnostics: VecDeque::new(),
            })),
            service: service.clone(),
            transport,
            limits,
            wake: Rc::new(Event::new()),
            running: Rc::new(Cell::new(false)),
            stopped: Rc::new(Cell::new(false)),
            execution_gate: DeviceExecutionGate::default(),
            completions: CommandCompletionLog {
                inner: Rc::new(RefCell::new((0, VecDeque::new()))),
                event: Rc::new(Event::new()),
            },
        };
        service.set_command_sink(Rc::new(RuntimeCommandSink {
            inner: Rc::downgrade(&runtime.inner),
            limits,
            wake: runtime.wake.clone(),
            stopped: runtime.stopped.clone(),
        }));
        runtime
    }

    pub fn execution_gate(&self) -> DeviceExecutionGate {
        self.execution_gate.clone()
    }

    pub fn subscribe_completions(&self) -> CommandCompletionSubscription {
        CommandCompletionSubscription {
            cursor: self.completions.inner.borrow().0,
            log: self.completions.clone(),
            lagged: false,
        }
    }

    pub fn register(&self, feature: RuntimeFeature) {
        let mut state = self.inner.borrow_mut();
        let identity = feature.identity.clone();
        let authority_changed = state.features.get(&identity).is_some_and(|previous| {
            previous.descriptor != feature.descriptor
                || previous.authority_generation != feature.authority_generation
                || previous.auth_session_generation != feature.auth_session_generation
        });
        if authority_changed {
            cancel_feature(&mut state, &identity, CommandOutcome::Cancelled);
        }
        state.features.insert(identity, feature);
        drop(state);
        revoke_invalid(&self.inner);
        self.wake.notify(usize::MAX);
    }

    pub fn unregister(&self, feature: &FeatureIdentity) {
        let mut state = self.inner.borrow_mut();
        state.features.remove(feature);
        cancel_feature(&mut state, feature, CommandOutcome::Cancelled);
        self.wake.notify(usize::MAX);
    }

    pub fn cancel_all(&self) {
        cancel_all(&mut self.inner.borrow_mut(), CommandOutcome::Cancelled);
        self.wake.notify(usize::MAX);
    }

    pub async fn run_until_idle(&self) {
        self.drive(true).await;
    }

    pub async fn run(&self) {
        self.drive(false).await;
    }

    pub fn stop(&self) {
        self.stopped.set(true);
        abort_all(&mut self.inner.borrow_mut(), CommandOutcome::Cancelled);
        self.wake.notify(usize::MAX);
    }

    async fn drive(&self, stop_when_idle: bool) {
        if self.stopped.get() || self.running.replace(true) {
            return;
        }
        let _run_guard = RunGuard {
            inner: self.inner.clone(),
            running: self.running.clone(),
            stopped: self.stopped.clone(),
            terminate_on_drop: !stop_when_idle,
        };
        let mut busy = HashSet::new();
        let mut running = FuturesUnordered::new();
        loop {
            let jobs = take_ready_jobs(&mut self.inner.borrow_mut(), &busy);
            for (device, job) in jobs {
                busy.insert(device.clone());
                running.push(async move { (device, self.execute(job).await) }.boxed_local());
            }
            if running.is_empty() {
                if stop_when_idle || self.stopped.get() {
                    return;
                }
                let listener = self.wake.listen();
                if self.inner.borrow().queued == 0 {
                    listener.await;
                }
                continue;
            }
            let listener = self.wake.listen();
            match future::race(running.next().map(Either::Completed), async {
                listener.await;
                Either::Woken
            })
            .await
            {
                Either::Completed(Some((device, ()))) => {
                    busy.remove(&device);
                    prune_generations(&mut self.inner.borrow_mut());
                }
                Either::Completed(None) | Either::Woken => {}
            }
        }
    }

    pub fn drain_diagnostics(&self) -> Vec<RuntimeDiagnostic> {
        self.inner.borrow_mut().diagnostics.drain(..).collect()
    }

    async fn execute(&self, batch: QueuedBatch) {
        self.inner.borrow_mut().reserved.remove(&batch.id);
        if batch.cancelled.get() || batch.reply.is_disconnected() {
            let _ = batch.reply.try_send(CommandOutcome::Cancelled);
            return;
        }
        let mut accepted = false;
        let mut superseded = false;
        for step in batch.steps {
            if batch.cancelled.get() || batch.reply.is_disconnected() {
                let _ = batch.reply.try_send(if accepted {
                    CommandOutcome::Accepted
                } else {
                    CommandOutcome::Cancelled
                });
                return;
            }
            let now = Instant::now();
            if now >= batch.deadline {
                let _ = batch.reply.try_send(CommandOutcome::Expired);
                return;
            }
            let Some(runtime_feature) = self.inner.borrow().features.get(&batch.feature).cloned()
            else {
                let _ = batch.reply.try_send(CommandOutcome::Unavailable);
                return;
            };
            if batch.authority
                != (QueuedAuthority {
                    authority_generation: runtime_feature.authority_generation,
                    auth_session_generation: runtime_feature.auth_session_generation,
                })
            {
                let _ = batch.reply.try_send(CommandOutcome::Cancelled);
                return;
            }
            if let Err(error) = self.service.validate_command(&batch.feature, &step.command) {
                let outcome = match error {
                    crate::device::ServiceCommandError::Unavailable => CommandOutcome::Unavailable,
                    crate::device::ServiceCommandError::UnknownFeature
                    | crate::device::ServiceCommandError::Invalid(_) => CommandOutcome::Unsupported,
                };
                let _ = batch.reply.try_send(outcome);
                return;
            }
            if self.step_is_superseded(&runtime_feature, &step) {
                superseded = true;
                continue;
            }
            let operations = match runtime_feature.descriptor.binding.encode(&step.command) {
                Ok(operations) => operations,
                Err(_) => {
                    let _ = batch.reply.try_send(CommandOutcome::Unsupported);
                    return;
                }
            };
            for operation in operations {
                if self.step_is_superseded(&runtime_feature, &step) {
                    superseded = true;
                    break;
                }
                if Instant::now() >= batch.deadline {
                    let _ = batch.reply.try_send(CommandOutcome::Expired);
                    return;
                }
                let queue_guard = self.guard(
                    &runtime_feature,
                    &step,
                    batch.cancelled.clone(),
                    batch.deadline,
                );
                *batch.send_state.borrow_mut() = Some(queue_guard.shared_state());
                self.inner.borrow_mut().active.insert(
                    batch.id,
                    ActiveSend {
                        device: batch.feature.physical.clone(),
                        feature: batch.feature.clone(),
                        guard: queue_guard.clone(),
                        cancelled: batch.cancelled.clone(),
                        reply: batch.reply.clone(),
                    },
                );
                let lease = future::race(
                    self.execution_gate
                        .acquire(batch.feature.physical.clone(), batch.deadline)
                        .map(Some),
                    async {
                        queue_guard.cancelled().await;
                        None
                    },
                )
                .await;
                let Some(_lease) = lease.flatten() else {
                    self.inner.borrow_mut().active.remove(&batch.id);
                    let outcome = if queue_guard.shared_state().expired() {
                        CommandOutcome::Expired
                    } else {
                        CommandOutcome::Cancelled
                    };
                    let _ = batch.reply.try_send(outcome);
                    return;
                };
                match queue_guard.authorization() {
                    SendAuthorization::Allowed => {}
                    SendAuthorization::Revoked => {
                        self.inner.borrow_mut().active.remove(&batch.id);
                        let outcome = if queue_guard.shared_state().expired() {
                            CommandOutcome::Expired
                        } else {
                            CommandOutcome::Cancelled
                        };
                        let _ = batch.reply.try_send(outcome);
                        return;
                    }
                }
                let available = self.transport.available_paths(&batch.feature.physical);
                let Some(path) = select_path(available) else {
                    self.inner.borrow_mut().active.remove(&batch.id);
                    let _ = batch.reply.try_send(CommandOutcome::Unavailable);
                    return;
                };
                let path_limit = if path == ControlPath::Cloud {
                    self.limits.cloud_attempt
                } else {
                    self.limits.local_attempt
                };
                let remaining = batch.deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.inner.borrow_mut().active.remove(&batch.id);
                    let _ = batch.reply.try_send(CommandOutcome::Expired);
                    return;
                }
                let attempt_deadline = batch.deadline.min(Instant::now() + path_limit);
                let guard = self.guard(
                    &runtime_feature,
                    &step,
                    batch.cancelled.clone(),
                    attempt_deadline,
                );
                match guard.authorization() {
                    SendAuthorization::Allowed => {}
                    SendAuthorization::Revoked => {
                        self.inner.borrow_mut().active.remove(&batch.id);
                        let outcome = if guard.shared_state().expired() {
                            CommandOutcome::Expired
                        } else {
                            CommandOutcome::Cancelled
                        };
                        let _ = batch.reply.try_send(outcome);
                        return;
                    }
                }
                *batch.send_state.borrow_mut() = Some(guard.shared_state());
                self.inner.borrow_mut().active.insert(
                    batch.id,
                    ActiveSend {
                        device: batch.feature.physical.clone(),
                        feature: batch.feature.clone(),
                        guard: guard.clone(),
                        cancelled: batch.cancelled.clone(),
                        reply: batch.reply.clone(),
                    },
                );
                let completed_operation = operation.clone();
                let send = self.transport.send(
                    path,
                    TransportCommand {
                        device: batch.feature.physical.clone(),
                        typed: step.command.clone(),
                        operation,
                    },
                    remaining.min(path_limit),
                    guard.clone(),
                );
                let result = future::race(
                    future::race(send.map(Some), async {
                        Timer::at(attempt_deadline).await;
                        None
                    })
                    .map(SendRace::Finished),
                    async {
                        guard.cancelled().await;
                        SendRace::Revoked
                    },
                )
                .await;
                self.inner.borrow_mut().active.remove(&batch.id);
                match result {
                    SendRace::Finished(Some(Ok(()))) if guard.may_have_been_sent() => {
                        accepted = true;
                        self.record_completion(CommandCompletion {
                            feature: batch.feature.clone(),
                            command: step.command.clone(),
                            operation: completed_operation.clone(),
                            authority_generation: runtime_feature.authority_generation,
                            auth_session_generation: runtime_feature.auth_session_generation,
                            synchronize_by: Instant::now() + Duration::from_secs(5),
                            outcome: CommandOutcome::Accepted,
                        });
                        *batch.send_state.borrow_mut() = None;
                    }
                    SendRace::Finished(Some(Ok(()))) => {
                        let _ = batch.reply.try_send(CommandOutcome::Cancelled);
                        return;
                    }
                    SendRace::Finished(Some(Err(TransportFailure::Rejected(code)))) => {
                        self.record_command_failure(
                            &batch.feature,
                            &step.command,
                            path,
                            CommandOutcome::Rejected(code),
                            guard.may_have_been_sent(),
                        );
                        let _ = batch.reply.try_send(CommandOutcome::Rejected(code));
                        return;
                    }
                    SendRace::Finished(Some(Err(TransportFailure::Ambiguous))) => {
                        self.record_command_failure(
                            &batch.feature,
                            &step.command,
                            path,
                            CommandOutcome::Ambiguous,
                            guard.may_have_been_sent(),
                        );
                        if guard.may_have_been_sent() {
                            self.record_completion(CommandCompletion {
                                feature: batch.feature.clone(),
                                command: step.command.clone(),
                                operation: completed_operation.clone(),
                                authority_generation: runtime_feature.authority_generation,
                                auth_session_generation: runtime_feature.auth_session_generation,
                                synchronize_by: Instant::now() + Duration::from_secs(5),
                                outcome: CommandOutcome::Ambiguous,
                            });
                        }
                        let _ = batch.reply.try_send(CommandOutcome::Ambiguous);
                        return;
                    }
                    SendRace::Finished(Some(Err(TransportFailure::Unavailable))) => {
                        let outcome = if guard.may_have_been_sent() {
                            CommandOutcome::Ambiguous
                        } else if guard.shared_state().expired() {
                            CommandOutcome::Expired
                        } else {
                            CommandOutcome::Unavailable
                        };
                        if outcome == CommandOutcome::Ambiguous {
                            self.record_completion(CommandCompletion {
                                feature: batch.feature.clone(),
                                command: step.command.clone(),
                                operation: completed_operation.clone(),
                                authority_generation: runtime_feature.authority_generation,
                                auth_session_generation: runtime_feature.auth_session_generation,
                                synchronize_by: Instant::now() + Duration::from_secs(5),
                                outcome: outcome.clone(),
                            });
                        }
                        self.record_command_failure(
                            &batch.feature,
                            &step.command,
                            path,
                            outcome.clone(),
                            guard.may_have_been_sent(),
                        );
                        let _ = batch.reply.try_send(outcome);
                        return;
                    }
                    SendRace::Finished(None) => {
                        guard.revoke();
                        let outcome = if guard.may_have_been_sent() {
                            CommandOutcome::Ambiguous
                        } else {
                            CommandOutcome::Expired
                        };
                        if outcome == CommandOutcome::Ambiguous {
                            self.record_completion(CommandCompletion {
                                feature: batch.feature.clone(),
                                command: step.command.clone(),
                                operation: completed_operation.clone(),
                                authority_generation: runtime_feature.authority_generation,
                                auth_session_generation: runtime_feature.auth_session_generation,
                                synchronize_by: Instant::now() + Duration::from_secs(5),
                                outcome: outcome.clone(),
                            });
                        }
                        self.record_command_failure(
                            &batch.feature,
                            &step.command,
                            path,
                            outcome.clone(),
                            guard.may_have_been_sent(),
                        );
                        let _ = batch.reply.try_send(outcome);
                        return;
                    }
                    SendRace::Revoked => {
                        let outcome = if guard.may_have_been_sent() {
                            CommandOutcome::Ambiguous
                        } else if guard.shared_state().expired() {
                            CommandOutcome::Expired
                        } else {
                            CommandOutcome::Cancelled
                        };
                        if outcome == CommandOutcome::Ambiguous {
                            self.record_completion(CommandCompletion {
                                feature: batch.feature.clone(),
                                command: step.command.clone(),
                                operation: completed_operation.clone(),
                                authority_generation: runtime_feature.authority_generation,
                                auth_session_generation: runtime_feature.auth_session_generation,
                                synchronize_by: Instant::now() + Duration::from_secs(5),
                                outcome: outcome.clone(),
                            });
                        }
                        self.record_command_failure(
                            &batch.feature,
                            &step.command,
                            path,
                            outcome.clone(),
                            guard.may_have_been_sent(),
                        );
                        let _ = batch.reply.try_send(outcome);
                        return;
                    }
                }
            }
        }
        let outcome = if accepted {
            CommandOutcome::Accepted
        } else if superseded {
            CommandOutcome::Superseded
        } else {
            CommandOutcome::Unavailable
        };
        let _ = batch.reply.try_send(outcome);
    }

    fn record_completion(&self, completion: CommandCompletion) {
        let mut log = self.completions.inner.borrow_mut();
        log.0 = log.0.saturating_add(1);
        let version = log.0;
        log.1.push_back((version, completion));
        if log.1.len() > 128 {
            log.1.pop_front();
        }
        drop(log);
        self.completions.event.notify(usize::MAX);
    }

    fn record_command_failure(
        &self,
        feature: &FeatureIdentity,
        command: &DeviceCommand,
        path: ControlPath,
        outcome: CommandOutcome,
        sent: bool,
    ) {
        push_diagnostic(
            &mut self.inner.borrow_mut(),
            RuntimeDiagnostic::CommandFailure {
                feature: feature.clone(),
                command: command.clone(),
                path,
                stage: match outcome {
                    CommandOutcome::Rejected(_) => CommandFailureStage::DeviceResponse,
                    _ if sent => CommandFailureStage::AfterSend,
                    _ => CommandFailureStage::BeforeSend,
                },
                outcome,
                sent,
            },
        );
    }

    fn step_is_superseded(&self, feature: &RuntimeFeature, step: &QueuedStep) -> bool {
        let state = self.inner.borrow();
        if let Some(property) = command_property(&step.command)
            && step.property_generation
                != latest_generation(state.property_generations.get(&(
                    feature.identity.clone(),
                    step.action_boundary,
                    property,
                )))
        {
            return true;
        }
        step.off_sensitive
            && state
                .off_generations
                .get(&(feature.identity.clone(), step.action_boundary))
                .and_then(|generations| latest_generation(Some(generations)))
                .unwrap_or(0)
                > step.off_generation
    }

    fn guard(
        &self,
        feature: &RuntimeFeature,
        step: &QueuedStep,
        cancelled: Rc<Cell<bool>>,
        deadline: Instant,
    ) -> SendGuard {
        let inner: Weak<RefCell<RuntimeState>> = Rc::downgrade(&self.inner);
        let identity = feature.identity.clone();
        let authority_generation = feature.authority_generation;
        let property = command_property(&step.command);
        let property_generation = step.property_generation;
        let action_boundary = step.action_boundary;
        let off_generation = step.off_generation;
        let off_sensitive = step.off_sensitive;
        let service = self.service.clone();
        SendGuard {
            shared: SharedSendState {
                state: Arc::new(AtomicU8::new(0)),
                deadline,
            },
            check: Rc::new(move || {
                if cancelled.get() {
                    return SendAuthorization::Revoked;
                }
                let Some(runtime) = inner.upgrade() else {
                    return SendAuthorization::Revoked;
                };
                if !service.can_control(&identity) {
                    return SendAuthorization::Revoked;
                }
                let state = runtime.borrow();
                let Some(current) = state.features.get(&identity) else {
                    return SendAuthorization::Revoked;
                };
                if current.authority_generation != authority_generation {
                    return SendAuthorization::Revoked;
                }
                if property.is_some_and(|property| {
                    state
                        .property_generations
                        .get(&(identity.clone(), action_boundary, property))
                        .and_then(|generations| latest_generation(Some(generations)))
                        != property_generation
                }) {
                    return SendAuthorization::Revoked;
                }
                if off_sensitive
                    && state
                        .off_generations
                        .get(&(identity.clone(), action_boundary))
                        .and_then(|generations| latest_generation(Some(generations)))
                        .unwrap_or(0)
                        > off_generation
                {
                    return SendAuthorization::Revoked;
                }
                SendAuthorization::Allowed
            }),
            cancelled: Rc::new(Event::new()),
        }
    }
}

struct RuntimeCommandSink {
    inner: Weak<RefCell<RuntimeState>>,
    limits: CommandLimits,
    wake: Rc<Event>,
    stopped: Rc<Cell<bool>>,
}

impl DeviceCommandSink for RuntimeCommandSink {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> LocalBoxFuture<'static, CommandOutcome> {
        let (reply, receiver) = flume::bounded(1);
        let cancelled = Rc::new(Cell::new(false));
        let send_state = Rc::new(RefCell::new(None));
        let Some(inner) = self.inner.upgrade() else {
            return async { CommandOutcome::Unavailable }.boxed_local();
        };
        let mut state = inner.borrow_mut();
        if self.stopped.get()
            || state
                .queued
                .saturating_add(state.active.len())
                .saturating_add(state.reserved.len())
                >= self.limits.queue_capacity
            || !state.features.contains_key(&feature)
        {
            return async { CommandOutcome::Unavailable }.boxed_local();
        }
        prune_generations(&mut state);
        let registered = state.features.get(&feature).expect("feature checked above");
        let authority = QueuedAuthority {
            authority_generation: registered.authority_generation,
            auth_session_generation: registered.auth_session_generation,
        };
        let id = state.next_job;
        state.next_job = state.next_job.saturating_add(1);
        let off_generation = state
            .off_generations
            .get(&(
                feature.clone(),
                state
                    .action_boundaries
                    .get(&feature.physical)
                    .copied()
                    .unwrap_or(0),
            ))
            .and_then(|generations| latest_generation(Some(generations)))
            .unwrap_or(0);
        let mut steps = Vec::with_capacity(commands.len());
        let mut powered_batch = false;
        for command in commands {
            let is_action = command_property(&command).is_none();
            if is_action {
                let boundary = state
                    .action_boundaries
                    .entry(feature.physical.clone())
                    .or_default();
                *boundary = boundary.saturating_add(1);
            }
            let action_boundary = state
                .action_boundaries
                .get(&feature.physical)
                .copied()
                .unwrap_or(0);
            let property_generation = command_property(&command).map(|property| {
                let generation = state.next_target;
                state.next_target = state.next_target.saturating_add(1);
                let generations = state
                    .property_generations
                    .entry((feature.clone(), action_boundary, property))
                    .or_default();
                generations.push(TargetGeneration {
                    generation,
                    cancelled: Rc::downgrade(&cancelled),
                });
                generation
            });
            let mut captured_off = off_generation;
            if matches!(command, DeviceCommand::SetPower(false)) {
                let generations = state
                    .off_generations
                    .entry((feature.clone(), action_boundary))
                    .or_default();
                let generation = generations
                    .last()
                    .map_or(1, |entry| entry.generation.saturating_add(1));
                generations.push(TargetGeneration {
                    generation,
                    cancelled: Rc::downgrade(&cancelled),
                });
                captured_off = generation;
            }
            if matches!(command, DeviceCommand::SetPower(true)) {
                powered_batch = true;
            }
            steps.push(QueuedStep {
                off_sensitive: powered_batch || step_off_sensitive(&command),
                command,
                property_generation,
                action_boundary,
                off_generation: captured_off,
            });
            if is_action {
                let boundary = state
                    .action_boundaries
                    .entry(feature.physical.clone())
                    .or_default();
                *boundary = boundary.saturating_add(1);
            }
        }
        let deadline = Instant::now() + self.limits.total;
        state
            .queues
            .entry(feature.physical.clone())
            .or_default()
            .push_back(QueuedBatch {
                id,
                feature,
                steps,
                deadline,
                cancelled: cancelled.clone(),
                send_state: send_state.clone(),
                reply,
                authority,
            });
        state.queued += 1;
        drop(state);
        revoke_invalid(&inner);
        prune_generations(&mut inner.borrow_mut());
        self.wake.notify(usize::MAX);
        let mut cancellation = TicketCancellation {
            cancelled: Some(cancelled),
            inner: inner.clone(),
            wake: self.wake.clone(),
            id,
            send_state,
        };
        async move {
            // Preserve an outcome the actor already recorded before the caller polled.
            let result = future::or(
                receiver
                    .recv_async()
                    .map(|result| TicketWait::Reply(result.unwrap_or(CommandOutcome::Cancelled))),
                async move {
                    Timer::at(deadline).await;
                    TicketWait::Deadline
                },
            )
            .await;
            let outcome = match result {
                TicketWait::Reply(outcome) => outcome,
                TicketWait::Deadline => cancellation.cancel_timeout(),
            };
            cancellation.disarm();
            outcome
        }
        .boxed_local()
    }

    fn stop_adjustment(&self, feature: &FeatureIdentity, property: Property) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.borrow_mut();
        let action_boundary = state
            .action_boundaries
            .get(&feature.physical)
            .copied()
            .unwrap_or(0);
        state
            .property_generations
            .remove(&(feature.clone(), action_boundary, property));
        drop(state);
        revoke_invalid(&inner);
        prune_generations(&mut inner.borrow_mut());
        self.wake.notify(usize::MAX);
    }
}

struct TicketCancellation {
    cancelled: Option<Rc<Cell<bool>>>,
    inner: Rc<RefCell<RuntimeState>>,
    wake: Rc<Event>,
    id: u64,
    send_state: Rc<RefCell<Option<SharedSendState>>>,
}

impl TicketCancellation {
    fn disarm(&mut self) {
        self.cancelled = None;
    }

    fn cancel(&mut self, outcome: CommandOutcome) -> CommandOutcome {
        let Some(cancelled) = self.cancelled.take() else {
            return outcome;
        };
        cancelled.set(true);
        if let Some(shared) = self.send_state.borrow().as_ref() {
            shared.close();
        }
        let outcome = if self
            .send_state
            .borrow()
            .as_ref()
            .is_some_and(SharedSendState::may_have_been_sent)
        {
            CommandOutcome::Ambiguous
        } else {
            outcome
        };
        cancel_job(&mut self.inner.borrow_mut(), self.id, outcome.clone());
        self.wake.notify(usize::MAX);
        outcome
    }

    fn cancel_timeout(&mut self) -> CommandOutcome {
        self.cancel(CommandOutcome::Expired)
    }
}

enum TicketWait {
    Reply(CommandOutcome),
    Deadline,
}

impl Drop for TicketCancellation {
    fn drop(&mut self) {
        let _ = self.cancel(CommandOutcome::Cancelled);
    }
}

impl Drop for CommandRuntime {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) == 1 {
            abort_all(&mut self.inner.borrow_mut(), CommandOutcome::Cancelled);
        }
    }
}

fn select_path(paths: OperationPaths) -> Option<ControlPath> {
    if paths.gateway {
        Some(ControlPath::Gateway)
    } else if paths.lan {
        Some(ControlPath::Lan)
    } else if paths.cloud {
        Some(ControlPath::Cloud)
    } else {
        None
    }
}

fn command_property(command: &DeviceCommand) -> Option<Property> {
    match command {
        DeviceCommand::SetPower(_) => Some(Property::Power),
        DeviceCommand::SetBrightness(_) => Some(Property::Brightness),
        DeviceCommand::SetColorTemperature(_) => Some(Property::ColorTemperature),
        DeviceCommand::SetColor(_) => Some(Property::Color),
        DeviceCommand::SetTargetTemperature(_) => Some(Property::TargetTemperature),
        DeviceCommand::SetHvacMode(_) => Some(Property::HvacMode),
        DeviceCommand::SetFanSpeed(_) => Some(Property::FanSpeed),
        DeviceCommand::SetSwingMode(_) => Some(Property::SwingMode),
        DeviceCommand::SetCurtainPosition(_) => Some(Property::CurtainTargetPosition),
        DeviceCommand::SetOscillation(_) => Some(Property::Oscillation),
        DeviceCommand::SetVacuumCleanMode(_) => Some(Property::VacuumCleanMode),
        DeviceCommand::StopCurtain
        | DeviceCommand::StartVacuum
        | DeviceCommand::StopVacuum
        | DeviceCommand::ReturnVacuumToDock => None,
    }
}

fn step_off_sensitive(command: &DeviceCommand) -> bool {
    matches!(
        command,
        DeviceCommand::SetPower(true) | DeviceCommand::SetBrightness(_)
    )
}

enum Either<T> {
    Completed(T),
    Woken,
}

enum SendRace {
    Finished(Option<Result<(), TransportFailure>>),
    Revoked,
}

struct RunGuard {
    inner: Rc<RefCell<RuntimeState>>,
    running: Rc<Cell<bool>>,
    stopped: Rc<Cell<bool>>,
    terminate_on_drop: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.terminate_on_drop {
            self.stopped.set(true);
            abort_all(&mut self.inner.borrow_mut(), CommandOutcome::Cancelled);
        }
        self.running.set(false);
    }
}

fn take_ready_jobs(
    state: &mut RuntimeState,
    busy: &HashSet<PhysicalDeviceId>,
) -> Vec<(PhysicalDeviceId, QueuedBatch)> {
    let devices = state
        .queues
        .keys()
        .filter(|device| !busy.contains(*device))
        .cloned()
        .collect::<Vec<_>>();
    let mut jobs = Vec::new();
    for device in devices {
        if let Some(job) = state.queues.get_mut(&device).and_then(VecDeque::pop_front) {
            state.queued = state.queued.saturating_sub(1);
            state.reserved.insert(
                job.id,
                ReservedBatch {
                    device: device.clone(),
                    feature: job.feature.clone(),
                    cancelled: job.cancelled.clone(),
                    send_state: job.send_state.clone(),
                    reply: job.reply.clone(),
                },
            );
            jobs.push((device.clone(), job));
        }
        if state.queues.get(&device).is_some_and(VecDeque::is_empty) {
            state.queues.remove(&device);
        }
    }
    jobs
}

fn cancel_job(state: &mut RuntimeState, id: u64, outcome: CommandOutcome) {
    for queue in state.queues.values_mut() {
        if let Some(index) = queue.iter().position(|batch| batch.id == id) {
            let batch = queue
                .remove(index)
                .expect("batch index came from this queue");
            state.queued = state.queued.saturating_sub(1);
            let _ = batch.reply.try_send(outcome);
            state.queues.retain(|_, queue| !queue.is_empty());
            prune_generations(state);
            return;
        }
    }
    if let Some(active) = state.active.get(&id) {
        let _ = active.reply.try_send(outcome);
        active.guard.revoke();
    } else if let Some(reserved) = state.reserved.remove(&id) {
        reserved.cancelled.set(true);
        let _ = reserved.reply.try_send(outcome);
    }
}

fn prune_generations(state: &mut RuntimeState) {
    state.property_generations.retain(|_, generations| {
        generations.retain(|entry| {
            entry
                .cancelled
                .upgrade()
                .is_some_and(|cancelled| !cancelled.get())
        });
        !generations.is_empty()
    });
    state.off_generations.retain(|_, generations| {
        generations.retain(|entry| {
            entry
                .cancelled
                .upgrade()
                .is_some_and(|cancelled| !cancelled.get())
        });
        !generations.is_empty()
    });
    let busy = state
        .queues
        .keys()
        .chain(state.active.values().map(|active| &active.device))
        .chain(state.reserved.values().map(|reserved| &reserved.device))
        .cloned()
        .collect::<HashSet<_>>();
    state
        .action_boundaries
        .retain(|device, _| busy.contains(device));
    state
        .property_generations
        .retain(|(feature, _, _), _| busy.contains(&feature.physical));
    state
        .off_generations
        .retain(|(feature, _), _| busy.contains(&feature.physical));
}

fn push_diagnostic(state: &mut RuntimeState, diagnostic: RuntimeDiagnostic) {
    const MAX_DIAGNOSTICS: usize = 32;
    if state.diagnostics.len() == MAX_DIAGNOSTICS {
        state.diagnostics.pop_front();
    }
    state.diagnostics.push_back(diagnostic);
}

fn revoke_invalid(inner: &Rc<RefCell<RuntimeState>>) {
    let guards = inner
        .borrow()
        .active
        .values()
        .map(|active| active.guard.clone())
        .collect::<Vec<_>>();
    for guard in guards {
        if !guard.permitted() {
            guard.revoke();
        }
    }
}

fn latest_generation(generations: Option<&Vec<TargetGeneration>>) -> Option<u64> {
    generations?.iter().rev().find_map(|entry| {
        let active = entry.cancelled.upgrade().is_some_and(|value| !value.get());
        active.then_some(entry.generation)
    })
}

fn cancel_all(state: &mut RuntimeState, outcome: CommandOutcome) {
    let devices = state.queues.keys().cloned().collect::<Vec<_>>();
    for device in devices {
        cancel_device(state, &device, outcome.clone());
    }
    for active in state.active.values() {
        active.cancelled.set(true);
        active.guard.revoke();
    }
    for (_, reserved) in state.reserved.drain() {
        reserved.cancelled.set(true);
        if let Some(shared) = reserved.send_state.borrow().as_ref() {
            shared.close();
        }
        let sent = reserved
            .send_state
            .borrow()
            .as_ref()
            .is_some_and(SharedSendState::may_have_been_sent);
        let reserved_outcome = if sent {
            CommandOutcome::Ambiguous
        } else {
            outcome.clone()
        };
        let _ = reserved.reply.try_send(reserved_outcome);
    }
    state.action_boundaries.clear();
    state.property_generations.clear();
    state.off_generations.clear();
}

fn abort_all(state: &mut RuntimeState, outcome: CommandOutcome) {
    cancel_all(state, outcome.clone());
    for (_, active) in state.active.drain() {
        let active_outcome = if active.guard.may_have_been_sent() {
            CommandOutcome::Ambiguous
        } else {
            outcome.clone()
        };
        let _ = active.reply.try_send(active_outcome);
    }
}

fn cancel_device(state: &mut RuntimeState, device: &PhysicalDeviceId, outcome: CommandOutcome) {
    if let Some(queue) = state.queues.remove(device) {
        state.queued = state.queued.saturating_sub(queue.len());
        for batch in queue {
            batch.cancelled.set(true);
            let _ = batch.reply.try_send(outcome.clone());
        }
    }
    for active in state
        .active
        .values()
        .filter(|active| &active.device == device)
    {
        active.cancelled.set(true);
        active.guard.revoke();
    }
    let reserved_ids = state
        .reserved
        .iter()
        .filter(|(_, reserved)| &reserved.device == device)
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    for id in reserved_ids {
        if let Some(reserved) = state.reserved.remove(&id) {
            reserved.cancelled.set(true);
            let _ = reserved.reply.try_send(outcome.clone());
        }
    }
}

fn cancel_feature(state: &mut RuntimeState, feature: &FeatureIdentity, outcome: CommandOutcome) {
    if let Some(queue) = state.queues.get_mut(&feature.physical) {
        let mut retained = VecDeque::new();
        while let Some(batch) = queue.pop_front() {
            if &batch.feature == feature {
                state.queued = state.queued.saturating_sub(1);
                batch.cancelled.set(true);
                let _ = batch.reply.try_send(outcome.clone());
            } else {
                retained.push_back(batch);
            }
        }
        *queue = retained;
    }
    state.queues.retain(|_, queue| !queue.is_empty());
    for active in state.active.values() {
        if &active.feature == feature {
            active.cancelled.set(true);
            active.guard.revoke();
        }
    }
    let reserved_ids = state
        .reserved
        .iter()
        .filter(|(_, reserved)| &reserved.feature == feature)
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    for id in reserved_ids {
        if let Some(reserved) = state.reserved.remove(&id) {
            reserved.cancelled.set(true);
            let _ = reserved.reply.try_send(outcome.clone());
        }
    }
}

#[cfg(test)]
mod tests;
