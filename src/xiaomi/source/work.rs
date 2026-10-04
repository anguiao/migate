use super::{
    model::SourceError,
    state::{Context, Lifetime},
};
use crate::{
    device::{CommandError, CommandReceiver, PhysicalDevice},
    storage::CachedPhysicalDevice,
    xiaomi::{
        capability::PropertyBinding,
        central::{CentralError, SessionHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    time::Duration,
};
pub(super) struct DeviceWork {
    pub(super) life: Lifetime,
    pub(super) cached: RefCell<CachedPhysicalDevice>,
    pub(super) physical: PhysicalDevice,
    pub(super) subscribed: RefCell<BTreeSet<(u32, u32)>>,
    subscribe_requested: Cell<bool>,
    subscribe_running: Cell<bool>,
    refresh_requested: Cell<bool>,
    changed: Event,
    subscribe_problem: RefCell<Option<String>>,
    read_problem: RefCell<Option<String>>,
}
impl DeviceWork {
    pub(super) fn new(cached: CachedPhysicalDevice, physical: PhysicalDevice) -> Self {
        Self {
            life: Lifetime::default(),
            cached: RefCell::new(cached),
            physical,
            subscribed: RefCell::new(BTreeSet::new()),
            subscribe_requested: Cell::new(true),
            subscribe_running: Cell::new(false),
            refresh_requested: Cell::new(true),
            changed: Event::new(),
            subscribe_problem: RefCell::new(None),
            read_problem: RefCell::new(None),
        }
    }
    pub(super) fn retry_subscriptions(&self) {
        if !self.subscribe_running.get() {
            self.subscribe_requested.set(true);
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn refresh(&self) {
        self.refresh_requested.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn update(&self, cached: CachedPhysicalDevice) {
        *self.cached.borrow_mut() = cached;
        self.subscribe_requested.set(true);
        self.refresh_requested.set(true);
        self.changed.notify(usize::MAX);
    }
    fn bindings(&self) -> Vec<PropertyBinding> {
        let mut values = self
            .cached
            .borrow()
            .device
            .functions
            .iter()
            .map(|f| f.power)
            .collect::<Vec<_>>();
        if let Some(trigger) = self.cached.borrow().device.refresh_trigger {
            values.push(trigger);
        }
        values
    }
    pub(super) fn diagnostic(&self) -> Option<String> {
        let values = [
            self.subscribe_problem.borrow().clone(),
            self.read_problem.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }
    fn report(&self, context: &Context, did: &str) {
        let blocked = context
            .live
            .borrow()
            .records
            .get(did)
            .and_then(|record| record.blocked.clone());
        context.problem(did, blocked.or_else(|| self.diagnostic()));
    }
    fn problem(&self, operation: &str, binding: PropertyBinding, error: &CentralError) -> String {
        let message = format!(
            "{} {operation} {}.{}: {error}",
            self.cached.borrow().device.description.key.id,
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
    generation: u64,
) -> Result<(), SourceError> {
    future::or(
        async {
            work.life.ended().await;
            Ok(())
        },
        future::or(
            subscriptions(&work, &handle, &context, generation),
            refreshes(&work, &handle, &context, generation),
        ),
    )
    .await
}
async fn subscriptions(
    work: &DeviceWork,
    handle: &SessionHandle,
    context: &Context,
    generation: u64,
) -> Result<(), SourceError> {
    let did = work.cached.borrow().device.description.key.id.clone();
    loop {
        let listener = work.changed.listen();
        if !work.subscribe_requested.replace(false) {
            listener.await;
            continue;
        }
        work.subscribe_running.set(true);
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
                if work.life.closed.get() || !context.session_current(generation) {
                    return Ok(());
                }
                match handle.subscribe_property(&did, binding).await {
                    Ok(()) => {
                        work.subscribed
                            .borrow_mut()
                            .insert((binding.siid, binding.piid));
                    }
                    Err(CentralError::Cancelled) => return Ok(()),
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
async fn refreshes(
    work: &Rc<DeviceWork>,
    handle: &SessionHandle,
    context: &Rc<Context>,
    generation: u64,
) -> Result<(), SourceError> {
    let did = work.cached.borrow().device.description.key.id.clone();
    loop {
        let listener = work.changed.listen();
        if !work.refresh_requested.replace(false) {
            listener.await;
            continue;
        }
        let mut pending = work.cached.borrow().device.functions.clone();
        for attempt in 0..4 {
            let mut failed = Vec::new();
            let mut problem = None;
            for function in pending {
                if !work
                    .cached
                    .borrow()
                    .device
                    .functions
                    .iter()
                    .any(|current| current.id == function.id && current.power == function.power)
                {
                    continue;
                }
                if work.life.closed.get() || !context.session_current(generation) {
                    return Ok(());
                }
                let observing = work.clone();
                let current = context.clone();
                let id = function.id.clone();
                let binding = function.power;
                let observing_id = id.clone();
                let result =
                    handle
                        .read_property(&did, binding, move |result| {
                            if current.session_current(generation)
                                && !observing.life.closed.get()
                                && observing.cached.borrow().device.functions.iter().any(
                                    |function| {
                                        function.id == observing_id && function.power == binding
                                    },
                                )
                                && let Ok(value) = result
                            {
                                observing
                                    .physical
                                    .function(&observing_id)
                                    .unwrap()
                                    .observe(value);
                            }
                        })
                        .await;
                match result {
                    Ok(_) => {}
                    Err(CentralError::Cancelled) => return Ok(()),
                    Err(error) => {
                        if work
                            .cached
                            .borrow()
                            .device
                            .functions
                            .iter()
                            .any(|current| current.id == id && current.power == binding)
                        {
                            problem = Some(work.problem("read", binding, &error));
                            failed.push(function);
                        }
                    }
                }
            }
            if failed.is_empty() || attempt == 3 {
                *work.read_problem.borrow_mut() = problem;
                work.report(context, &did);
                break;
            }
            pending = failed;
            context
                .clock
                .wait_until(context.clock.now() + Duration::from_secs(3))
                .await;
        }
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
pub(super) async fn consume(
    mut receiver: CommandReceiver,
    context: Rc<Context>,
    did: String,
    function_id: String,
) -> Result<(), SourceError> {
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
        let binding = context.live.borrow().records.get(&did).and_then(|record| {
            record
                .cached
                .device
                .functions
                .iter()
                .find(|function| function.id == function_id)
                .map(|function| function.power)
        });
        let Some(binding) = binding else {
            request.reject(CommandError::Cancelled);
            continue;
        };
        let generation = handle.generation();
        let current = context.clone();
        let target = request.target;
        let command_did = did.clone();
        let command_function = function_id.clone();
        let result = handle.set_property(&did, binding, target, move |result| {
            if !current.session_current(generation)
                || !current
                    .live
                    .borrow()
                    .records
                    .get(&command_did)
                    .is_some_and(|record| {
                        record.cached.device.functions.iter().any(|function| {
                            function.id == command_function && function.power == binding
                        })
                    })
            {
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
