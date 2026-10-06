use super::{
    model::AdapterError,
    observations::Observations,
    refresh::{ReadFlight, Refresh},
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
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    time::Duration,
};
pub(super) struct DeviceWork {
    // Binding work ends on incompatibility, while the physical device keeps its consumers.
    pub(super) life: Lifetime,
    pub(super) observations: Observations,
    pub(super) record: Rc<DeviceRecord>,
    pub(super) subscribed: RefCell<BTreeSet<(u32, u32)>>,
    pub(super) active: RefCell<Option<super::notifications::NotificationSource>>,
    pub(super) local_generation: Cell<u64>,
    pub(super) local_exhausted: Cell<bool>,
    pub(super) cloud_subscribed: RefCell<BTreeSet<(u32, u32)>>,
    pub(super) cloud_generation: Cell<u64>,
    pub(super) cloud_running: Cell<bool>,
    pub(super) cloud_exhausted: Cell<bool>,
    pub(super) cloud_problem: RefCell<Option<String>>,
    pub(super) condition_generation: Cell<u64>,
    subscribe_requested: Cell<bool>,
    pub(super) subscribe_running: Cell<bool>,
    pub(super) refresh_pending: RefCell<VecDeque<Refresh>>,
    pub(super) refresh_inflight: RefCell<BTreeMap<String, Rc<ReadFlight>>>,
    pub(super) refresh_changed: Rc<Event>,
    changed: Event,
    subscribe_problem: RefCell<Option<String>>,
    pub(super) read_problems: RefCell<BTreeMap<String, String>>,
}
impl DeviceWork {
    pub(super) fn new(record: Rc<DeviceRecord>, refresh_changed: Rc<Event>) -> Self {
        let work = Self {
            life: Lifetime::default(),
            observations: Observations::default(),
            record,
            subscribed: RefCell::new(BTreeSet::new()),
            active: RefCell::new(None),
            local_generation: Cell::new(0),
            local_exhausted: Cell::new(false),
            cloud_subscribed: RefCell::new(BTreeSet::new()),
            cloud_generation: Cell::new(0),
            cloud_running: Cell::new(false),
            cloud_exhausted: Cell::new(false),
            cloud_problem: RefCell::new(None),
            condition_generation: Cell::new(0),
            subscribe_requested: Cell::new(false),
            subscribe_running: Cell::new(false),
            refresh_pending: RefCell::new(VecDeque::new()),
            refresh_inflight: RefCell::new(BTreeMap::new()),
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
        self.subscribe_requested.set(false);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn source_changed(&self) {
        self.observations.source_changed();
        super::refresh::cancel(self);
        self.refresh();
    }
    pub(super) fn local_disconnected(&self) {
        if matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Local(_))
        ) {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.local_generation.set(self.local_generation.get() + 1);
        self.local_exhausted.set(false);
        self.subscribed.borrow_mut().clear();
        self.subscribe_running.set(false);
        *self.subscribe_problem.borrow_mut() = None;
    }
    pub(super) fn cloud_disconnected(&self) {
        if matches!(
            *self.active.borrow(),
            Some(super::notifications::NotificationSource::Cloud(_))
        ) {
            self.active.borrow_mut().take();
            self.source_changed();
        }
        self.cloud_generation.set(self.cloud_generation.get() + 1);
        self.cloud_subscribed.borrow_mut().clear();
        self.cloud_running.set(false);
        self.cloud_exhausted.set(false);
        self.cloud_problem.borrow_mut().take();
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
    pub(super) fn request_local_subscriptions(&self) {
        if !self.subscribe_running.get() && !self.local_exhausted.get() {
            self.subscribe_requested.set(true);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn retry_subscriptions(&self) {
        self.condition_generation
            .set(self.condition_generation.get() + 1);
        self.local_exhausted.set(false);
        self.cloud_exhausted.set(false);
        self.changed.notify(usize::MAX);
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
        self.active.borrow_mut().take();
        self.local_generation.set(self.local_generation.get() + 1);
        self.cloud_disconnected();
        self.source_changed();
        self.subscribed.borrow_mut().clear();
        self.local_exhausted.set(false);
        self.subscribe_requested.set(true);
        self.refresh_pending.borrow_mut().clear();
        self.read_problems.borrow_mut().clear();
        self.refresh();
        self.changed.notify(usize::MAX);
    }
    pub(super) fn bindings(&self) -> Vec<PropertyBinding> {
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
            .chain(self.cloud_problem.borrow().clone())
            .chain(self.read_problems.borrow().values().cloned())
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    pub(super) fn report(&self, context: &Context, did: &str) {
        if self.life.is_closed() {
            return;
        }
        let blocked = context.live.borrow().devices[did].blocked.clone();
        context.problem(did, blocked.or_else(|| self.diagnostic()));
    }
    pub(super) fn problem(
        &self,
        operation: &str,
        binding: PropertyBinding,
        error: &CentralError,
    ) -> String {
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
        if !matches!(context.notification_source(&did), Some(super::notifications::NotificationSource::Local(selected)) if selected.same_connection(handle))
        {
            continue;
        }
        work.subscribe_running.set(true);
        let generation = work.record.binding_generation.get();
        let preparation = work.local_generation.get();
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
                if !context.current_local(handle)
                    || !matches!(context.notification_source(&did), Some(super::notifications::NotificationSource::Local(selected)) if selected.same_connection(handle))
                    || work.local_generation.get() != preparation
                    || work.record.binding_generation.get() != generation
                    || !work.bindings().contains(&binding)
                {
                    continue;
                }
                match handle.subscribe_property(&did, binding).await {
                    Ok(())
                        if context.current_local(handle)
                            && work.local_generation.get() == preparation
                            && work.record.binding_generation.get() == generation =>
                    {
                        work.subscribed
                            .borrow_mut()
                            .insert((binding.siid, binding.piid));
                    }
                    Ok(()) => {}
                    Err(error) => {
                        if context.current_local(handle)
                            && work.local_generation.get() == preparation
                            && work.record.binding_generation.get() == generation
                            && work.bindings().contains(&binding)
                        {
                            problem = Some(work.problem("subscribe", binding, &error));
                            failed.push(binding);
                        }
                    }
                }
            }
            if !context.current_local(handle)
                || work.local_generation.get() != preparation
                || work.record.binding_generation.get() != generation
            {
                break;
            }
            if failed.is_empty() || attempt == 3 {
                work.local_exhausted.set(problem.is_some());
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
        context.changed.notify(usize::MAX);
    }
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
