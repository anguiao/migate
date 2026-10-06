use super::{
    model::AdapterError,
    observations::Observations,
    routing::ControlPath,
    state::{Context, DeviceRecord, Lifetime},
};
use crate::{
    device::{CommandError, CommandReceiver},
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, RequestStage, SendDecision, SendHooks, SessionHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use futures_util::future::join_all;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};
const REFRESH_DELAY: Duration = Duration::from_millis(200);
const READ_TIMEOUT: Duration = Duration::from_secs(6);
const RETRY_DELAY: Duration = Duration::from_secs(3);

struct Refresh {
    id: String,
    binding: PropertyBinding,
    attempts: u8,
    ready_at: Duration,
}
pub(super) struct DeviceWork {
    // Binding work ends on incompatibility, while the physical device keeps its consumers.
    pub(super) life: Lifetime,
    pub(super) observations: Observations,
    pub(super) record: Rc<DeviceRecord>,
    pub(super) subscribed: RefCell<BTreeSet<(u32, u32)>>,
    subscribe_requested: Cell<bool>,
    subscribe_running: Cell<bool>,
    refresh_pending: RefCell<VecDeque<Refresh>>,
    refresh_changed: Rc<Event>,
    changed: Event,
    subscribe_problem: RefCell<Option<String>>,
    read_problems: RefCell<BTreeMap<String, String>>,
}
impl DeviceWork {
    pub(super) fn new(record: Rc<DeviceRecord>, refresh_changed: Rc<Event>) -> Self {
        let work = Self {
            life: Lifetime::default(),
            observations: Observations::default(),
            record,
            subscribed: RefCell::new(BTreeSet::new()),
            subscribe_requested: Cell::new(true),
            subscribe_running: Cell::new(false),
            refresh_pending: RefCell::new(VecDeque::new()),
            refresh_changed,
            changed: Event::new(),
            subscribe_problem: RefCell::new(None),
            read_problems: RefCell::new(BTreeMap::new()),
        };
        work.refresh();
        work
    }
    pub(super) fn local_connected(&self) {
        self.local_disconnected();
        self.subscribe_requested.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn local_disconnected(&self) {
        self.observations.source_changed();
        self.subscribed.borrow_mut().clear();
        self.subscribe_running.set(false);
        *self.subscribe_problem.borrow_mut() = None;
        self.refresh();
    }
    pub(super) fn notify(&self, binding: PropertyBinding, value: bool) {
        if self.life.is_closed() {
            return;
        }
        let cached = self.record.cached.borrow();
        if cached.device.refresh_trigger == Some(binding) {
            drop(cached);
            self.refresh();
        } else if let Some(function) = cached
            .device
            .functions
            .iter()
            .find(|function| function.power == binding)
        {
            self.observations
                .notify(&self.record, &function.description.id, value);
        }
    }
    pub(super) fn retry_subscriptions(&self) {
        if !self.subscribe_running.get() {
            self.subscribe_requested.set(true);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn refresh(&self) {
        let mut pending = self.refresh_pending.borrow_mut();
        for function in &self.record.cached.borrow().device.functions {
            self.observations.refresh(&function.description.id);
            if !pending
                .iter()
                .any(|read| read.id == function.description.id)
            {
                pending.push_back(Refresh {
                    id: function.description.id.clone(),
                    binding: function.power,
                    attempts: 0,
                    ready_at: Duration::ZERO,
                });
            }
        }
        self.refresh_changed.notify(usize::MAX);
    }
    pub(super) fn bindings_changed(&self) {
        self.observations.source_changed();
        self.subscribed.borrow_mut().clear();
        self.subscribe_requested.set(true);
        self.refresh_pending.borrow_mut().clear();
        self.read_problems.borrow_mut().clear();
        self.refresh();
        self.changed.notify(usize::MAX);
    }
    fn bindings(&self) -> Vec<PropertyBinding> {
        let mut values = self
            .record
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .map(|f| f.power)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.record.cached.borrow().device.refresh_trigger {
            values.push(trigger);
        }
        values
    }
    pub(super) fn diagnostic(&self) -> Option<String> {
        let values = self
            .subscribe_problem
            .borrow()
            .clone()
            .into_iter()
            .chain(self.read_problems.borrow().values().cloned())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    fn report(&self, context: &Context, did: &str) {
        if self.life.is_closed() {
            return;
        }
        let blocked = context.live.borrow().devices[did].blocked.clone();
        context.problem(did, blocked.or_else(|| self.diagnostic()));
    }
    fn problem(&self, operation: &str, binding: PropertyBinding, error: &CentralError) -> String {
        let message = format!(
            "{} {operation} {}.{}: {error}",
            self.record.cached.borrow().device.key.id,
            binding.siid,
            binding.piid
        );
        log::warn!("{message}");
        message
    }
}
pub(super) async fn run(
    work: Rc<DeviceWork>,
    handle: SessionHandle,
    context: Rc<Context>,
) -> Result<(), AdapterError> {
    // Poll cancellation first so removed work cannot submit another request.
    future::or(
        async {
            work.life.ended().await;
            Ok(())
        },
        subscriptions(&work, &handle, &context),
    )
    .await
}
async fn subscriptions(
    work: &DeviceWork,
    handle: &SessionHandle,
    context: &Context,
) -> Result<(), AdapterError> {
    let did = work.record.cached.borrow().device.key.id.clone();
    loop {
        let listener = work.changed.listen();
        if !work.subscribe_requested.replace(false) {
            listener.await;
            continue;
        }
        work.subscribe_running.set(true);
        let generation = work.record.binding_generation.get();
        context
            .clock
            .wait_until(context.clock.now() + Duration::from_millis(10))
            .await;
        let mut pending = work
            .bindings()
            .into_iter()
            .filter(|b| !work.subscribed.borrow().contains(&(b.siid, b.piid)))
            .collect::<Vec<_>>();
        for attempt in 0..4 {
            let mut failed = Vec::new();
            let mut problem = None;
            for binding in pending {
                if work.record.binding_generation.get() != generation
                    || !work.bindings().contains(&binding)
                {
                    continue;
                }
                match handle.subscribe_property(&did, binding).await {
                    Ok(()) if work.record.binding_generation.get() == generation => {
                        work.subscribed
                            .borrow_mut()
                            .insert((binding.siid, binding.piid));
                    }
                    Ok(()) => {}
                    Err(error) => {
                        if work.record.binding_generation.get() == generation
                            && work.bindings().contains(&binding)
                        {
                            problem = Some(work.problem("subscribe", binding, &error));
                            failed.push(binding);
                        }
                    }
                }
            }
            if work.record.binding_generation.get() != generation {
                break;
            }
            if failed.is_empty() || attempt == 3 {
                *work.subscribe_problem.borrow_mut() = problem;
                work.report(context, &did);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_millis(100))
                .await;
        }
        work.subscribe_running.set(false);
    }
}
pub(super) async fn refreshes(
    context: Rc<Context>,
    handle: SessionHandle,
) -> Result<(), AdapterError> {
    let changed = context
        .live
        .borrow()
        .session
        .as_ref()
        .unwrap()
        .refresh_changed
        .clone();
    let mut next_round = None;
    loop {
        let listener = changed.listen();
        if context.priority() == super::routing::PathPriority::CloudFirst {
            next_round = None;
            listener.await;
            continue;
        }
        let works = context
            .live
            .borrow()
            .session
            .as_ref()
            .unwrap()
            .devices
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let ready_at = works
            .iter()
            .filter_map(|work| {
                work.refresh_pending
                    .borrow()
                    .iter()
                    .map(|read| read.ready_at)
                    .min()
            })
            .min();
        let Some(ready_at) = ready_at else {
            next_round = None;
            listener.await;
            continue;
        };
        let deadline = next_round
            .map_or(
                context.clock.now() + REFRESH_DELAY,
                |scheduled: Duration| scheduled.min(context.clock.now() + REFRESH_DELAY),
            )
            .max(ready_at);
        next_round = Some(deadline);
        future::or(context.clock.wait_until(deadline), async {
            listener.await;
        })
        .await;
        if context.clock.now() < deadline {
            continue;
        }
        if context.priority() == super::routing::PathPriority::CloudFirst {
            continue;
        }
        // Re-read membership after waiting: removed devices cannot enter the round.
        let round = context
            .live
            .borrow()
            .session
            .as_ref()
            .unwrap()
            .devices
            .values()
            .filter_map(|work| {
                if !context.local_reads_allowed(&work.record.physical.key().id, &handle) {
                    return None;
                }
                let mut pending = work.refresh_pending.borrow_mut();
                let index = pending
                    .iter()
                    .position(|read| read.ready_at <= context.clock.now())?;
                Some((work.clone(), pending.remove(index).unwrap()))
            })
            .collect::<Vec<_>>();
        // Match the upstream round barrier: one property per DID, concurrent DIDs.
        join_all(
            round
                .into_iter()
                .map(|(work, read)| read_one(work, read, handle.clone(), context.clone())),
        )
        .await;
        next_round = Some(context.clock.now() + REFRESH_DELAY);
    }
}
async fn read_one(
    work: Rc<DeviceWork>,
    mut read: Refresh,
    handle: SessionHandle,
    context: Rc<Context>,
) {
    future::or(
        async {
            work.life.ended().await;
        },
        async {
            let did = work.record.physical.key().id.clone();
            let binding = read.binding;
            let id = read.id.clone();
            if work.record.binding(&id) != binding {
                return;
            }
            let generation = work.record.binding_generation.get();
            let stamp = Rc::new(work.observations.read_stamp(&work.record, &id));
            let observing = work.clone();
            let observing_id = id.clone();
            let guard_work = work.clone();
            let guard_stamp = stamp.clone();
            let guard_context = context.clone();
            let guard_handle = handle.clone();
            let guard_id = id.clone();
            let guard_did = did.clone();
            let result_context = context.clone();
            let result_handle = handle.clone();
            let result = handle
                .read_property_with_timeout_guarded(
                    &did,
                    binding,
                    READ_TIMEOUT,
                    SendHooks::new(
                        move || {
                            if guard_work.life.is_closed()
                                || guard_work.record.life.is_closed()
                                || guard_work.record.binding_generation.get() != generation
                                || !guard_context.current_local(&guard_handle)
                            {
                                SendDecision::Cancel
                            } else if !guard_context.local_reads_allowed(&guard_did, &guard_handle)
                                || !guard_work.observations.read_is_current(
                                    &guard_work.record,
                                    &guard_id,
                                    &guard_stamp,
                                )
                            {
                                SendDecision::Withdraw
                            } else {
                                SendDecision::Send
                            }
                        },
                        || {},
                    ),
                    move |outcome| {
                        if !observing.life.is_closed()
                            && observing.record.binding_generation.get() == generation
                        {
                            result_context.local_outcome(&result_handle, &outcome);
                        }
                        if !observing.life.is_closed()
                            && result_context.current_local(&result_handle)
                            && let Ok(value) = outcome.result
                        {
                            observing.observations.observe_read(
                                &observing.record,
                                &observing_id,
                                &stamp,
                                value,
                            );
                        }
                    },
                )
                .await;
            if work.life.is_closed()
                || work.record.binding_generation.get() != generation
                || work.record.binding(&id) != binding
            {
                return;
            }
            match result.result {
                Ok(_) => {
                    work.read_problems.borrow_mut().remove(&id);
                }
                Err(CentralError::Withdrawn | CentralError::Cancelled)
                    if result.stage == RequestStage::NotSent =>
                {
                    let mut pending = work.refresh_pending.borrow_mut();
                    if !pending.iter().any(|queued| queued.id == id) {
                        pending.push_back(read);
                    }
                }
                Err(error) => {
                    let problem = work.problem("read", binding, &error);
                    if context.priority() == super::routing::PathPriority::CloudFirst {
                        work.read_problems
                            .borrow_mut()
                            .insert(id.clone(), problem.clone());
                    }
                    let mut pending = work.refresh_pending.borrow_mut();
                    if read.attempts == 3 {
                        pending.retain(|queued| queued.id != id);
                        work.read_problems.borrow_mut().insert(id, problem);
                    } else {
                        read.attempts += 1;
                        read.ready_at = context.clock.now() + RETRY_DELAY;
                        // A trigger during this read merges into its retry, retaining backoff.
                        if let Some(queued) = pending.iter_mut().find(|queued| queued.id == id) {
                            *queued = read;
                        } else {
                            pending.push_back(read);
                        }
                    }
                }
            }
            work.report(&context, &did);
        },
    )
    .await;
}
fn command_error(error: CentralError) -> CommandError {
    match error {
        CentralError::Timeout => CommandError::Timeout,
        CentralError::Rejected(code) => CommandError::Rejected(format!("gateway code {code}")),
        CentralError::Cancelled => CommandError::Cancelled,
        error => CommandError::SendFailed(error.to_string()),
    }
}
async fn send_local_write(
    context: &Rc<Context>,
    handle: SessionHandle,
    work: Rc<DeviceWork>,
    id: &str,
    binding: PropertyBinding,
    generation: u64,
    request: Rc<RefCell<Option<crate::device::CommandRequest>>>,
) -> bool {
    let did = work.record.physical.key().id.clone();
    let target = request.borrow().as_ref().unwrap().target;
    let observing = work.clone();
    let id = id.to_owned();
    let stamp = Rc::new(RefCell::new(None));
    let capture_stamp = stamp.clone();
    let capture_work = work.clone();
    let capture_id = id.clone();
    let guard_work = work.clone();
    let guard_context = context.clone();
    let guard_handle = handle.clone();
    let guard_did = did.clone();
    let result_context = context.clone();
    let result_handle = handle.clone();
    let outcome = handle.set_property_guarded(
        &did, binding, target,
        SendHooks::new(move || {
            if guard_work.life.is_closed() || guard_work.record.life.is_closed()
                || guard_work.record.binding_generation.get() != generation
                || !guard_context.current_local(&guard_handle)
            {
                SendDecision::Cancel
            } else if matches!(guard_context.control_path(&guard_did), Some(ControlPath::Local(selected)) if selected.same_connection(&guard_handle)) {
                SendDecision::Send
            } else {
                SendDecision::Withdraw
            }
        }, move || {
            *capture_stamp.borrow_mut() = Some(capture_work.observations.write_stamp(&capture_work.record, &capture_id));
        }),
        move |outcome| {
            if !observing.life.is_closed()
                && observing.record.binding_generation.get() == generation
            {
                result_context.local_outcome(&result_handle, &outcome);
            }
            if outcome.result == Err(CentralError::Withdrawn) && outcome.stage == RequestStage::NotSent {
                return;
            }
            let request = request.borrow_mut().take().unwrap();
            match outcome.result {
                Ok(()) => {
                    if let Some(stamp) = stamp.borrow_mut().take() {
                        observing.observations.accept_write(&observing.record, &id, &stamp, request);
                    } else {
                        request.reject(CommandError::SendFailed("gateway accepted an unsent write".into()));
                    }
                }
                Err(error) => request.reject(command_error(error)),
            }
        },
    ).await;
    outcome.result == Err(CentralError::Withdrawn) && outcome.stage == RequestStage::NotSent
}

pub(super) async fn consume(
    mut receiver: CommandReceiver,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    function_id: String,
) -> Result<(), AdapterError> {
    future::or(
        async {
            record.life.ended().await;
            Ok(())
        },
        async {
            let did = record.physical.key().id.clone();
            while let Some(request) = receiver.next().await {
                let work = context
                    .live
                    .borrow()
                    .devices
                    .get(&did)
                    .and_then(|slot| slot.work.clone());
                let path = context.control_path(&did);
                let (Some(work), Some(path)) = (work, path) else {
                    request.reject(CommandError::Unavailable("no usable control path".into()));
                    continue;
                };
                let binding = record.binding(&function_id);
                let target = request.target;
                let generation = record.binding_generation.get();
                let request = Rc::new(RefCell::new(Some(request)));
                let mut path = path;
                loop {
                    if work.life.is_closed()
                        || record.life.is_closed()
                        || record.binding_generation.get() != generation
                    {
                        request
                            .borrow_mut()
                            .take()
                            .unwrap()
                            .reject(CommandError::Cancelled);
                        break;
                    }
                    match path {
                        ControlPath::Local(handle) => {
                            if send_local_write(
                                &context,
                                handle,
                                work.clone(),
                                &function_id,
                                binding,
                                generation,
                                request.clone(),
                            )
                            .await
                            {
                                let Some(selected) = context.control_path(&did) else {
                                    request.borrow_mut().take().unwrap().reject(
                                        CommandError::Unavailable("no usable control path".into()),
                                    );
                                    break;
                                };
                                path = selected;
                                continue;
                            }
                        }
                        ControlPath::Cloud(credentials) => {
                            let stamp = work.observations.write_stamp(&record, &function_id);
                            match context
                                .cloud
                                .set_property(
                                    &credentials.tokens.access_token,
                                    &did,
                                    binding,
                                    target,
                                )
                                .await
                            {
                                Ok(()) => work.observations.accept_write(
                                    &record,
                                    &function_id,
                                    &stamp,
                                    request.borrow_mut().take().unwrap(),
                                ),
                                Err(error) => {
                                    request
                                        .borrow_mut()
                                        .take()
                                        .unwrap()
                                        .reject(match error.kind() {
                                        crate::xiaomi::cloud::CloudErrorKind::Timeout => {
                                            CommandError::Timeout
                                        }
                                        crate::xiaomi::cloud::CloudErrorKind::Business(code)
                                        | crate::xiaomi::cloud::CloudErrorKind::PropertyRejected(
                                            code,
                                        ) => CommandError::Rejected(format!("cloud code {code}")),
                                        _ => CommandError::SendFailed(error.to_string()),
                                    });
                                    context.cloud_error(
                                        &credentials.tokens.access_token,
                                        Some(&did),
                                        &error,
                                    )?;
                                }
                            }
                        }
                    }
                    break;
                }
            }
            Ok(())
        },
    )
    .await
}
