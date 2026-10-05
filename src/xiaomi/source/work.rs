use super::{
    model::SourceError,
    state::{Context, DeviceRecord, Lifetime},
};
use crate::{
    device::{CommandError, CommandReceiver},
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, SessionHandle},
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
    pub(super) life: Lifetime,
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
    pub(super) fn retry_subscriptions(&self) {
        if !self.subscribe_running.get() {
            self.subscribe_requested.set(true);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn refresh(&self) {
        let mut pending = self.refresh_pending.borrow_mut();
        for function in &self.record.cached.borrow().device.functions {
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
) -> Result<(), SourceError> {
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
) -> Result<(), SourceError> {
    let did = work.record.cached.borrow().device.key.id.clone();
    loop {
        let listener = work.changed.listen();
        if !work.subscribe_requested.replace(false) {
            listener.await;
            continue;
        }
        work.subscribe_running.set(true);
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
                if !work.bindings().contains(&binding) {
                    continue;
                }
                match handle.subscribe_property(&did, binding).await {
                    Ok(()) => {
                        work.subscribed
                            .borrow_mut()
                            .insert((binding.siid, binding.piid));
                    }
                    Err(error) => {
                        if work.bindings().contains(&binding) {
                            problem = Some(work.problem("subscribe", binding, &error));
                            failed.push(binding);
                        }
                    }
                }
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
) -> Result<(), SourceError> {
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
                .map(|(work, read)| read_one(work, read, &handle, &context)),
        )
        .await;
        next_round = Some(context.clock.now() + REFRESH_DELAY);
    }
}
async fn read_one(
    work: Rc<DeviceWork>,
    mut read: Refresh,
    handle: &SessionHandle,
    context: &Context,
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
            let observing = work.clone();
            let observing_id = id.clone();
            let result = handle
                .read_property_with_timeout(&did, binding, READ_TIMEOUT, move |result| {
                    if observing.record.binding(&observing_id) == binding
                        && let Ok(value) = result
                    {
                        observing
                            .record
                            .physical
                            .function(&observing_id)
                            .unwrap()
                            .observe(value);
                    }
                })
                .await;
            if work.record.binding(&id) != binding {
                return;
            }
            match result {
                Ok(_) => {
                    work.read_problems.borrow_mut().remove(&id);
                }
                Err(error) => {
                    let problem = work.problem("read", binding, &error);
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
            work.report(context, &did);
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
pub(super) async fn consume(
    mut receiver: CommandReceiver,
    context: Rc<Context>,
    record: Rc<DeviceRecord>,
    function_id: String,
) -> Result<(), SourceError> {
    let did = record.physical.key().id.clone();
    while let Some(request) = receiver.next().await {
        let session = context
            .live
            .borrow()
            .session
            .as_ref()
            .map(|session| session.handle.clone());
        let Some(handle) = session else {
            request.reject(CommandError::Unavailable("gateway is disconnected".into()));
            continue;
        };
        let binding = record.binding(&function_id);
        let target = request.target;
        let observing = record.clone();
        let command_function = function_id.clone();
        let result = handle.set_property(&did, binding, target, move |result| {
            if observing.binding(&command_function) != binding {
                request.reject(CommandError::Cancelled);
            } else {
                match result {
                    Ok(()) => {
                        let _ = request.accept();
                    }
                    Err(error) => request.reject(command_error(error)),
                }
            }
        });
        let _ = result.await;
    }
    Ok(())
}
