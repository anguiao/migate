use super::{
    model::{GatewayStatus, PendingDevice, Ready, SourceError, SourceStatus},
    work::DeviceWork,
};
use crate::{
    device::{Availability, Clock, CommandReceiver, DeviceKey, DeviceRegistry, PhysicalDevice},
    storage::{AdmissionError, CachedPhysicalDevice, CachedScope, Store, XiaomiRecord},
    xiaomi::{
        auth::BridgePreparation,
        central::{DeviceAccess, DeviceEntry, SessionHandle},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::Poll,
    time::Duration,
};
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(30);
pub(super) type Task = Pin<Box<dyn Future<Output = Result<(), SourceError>>>>;
#[derive(Default)]
pub(super) struct Lifetime {
    pub(super) closed: Cell<bool>,
    pub(super) changed: Event,
}
impl Lifetime {
    pub(super) fn close(&self) {
        self.closed.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(super) async fn ended(&self) {
        loop {
            let listener = self.changed.listen();
            if self.closed.get() {
                return;
            }
            listener.await;
        }
    }
}
pub(super) struct DeviceRecord {
    pub(super) cached: CachedPhysicalDevice,
    pub(super) physical: PhysicalDevice,
    pub(super) blocked: Option<String>,
}
pub(super) struct Session {
    pub(super) handle: SessionHandle,
    pub(super) access: AccessRefresh,
    pub(super) devices: BTreeMap<String, Rc<DeviceWork>>,
    pub(super) tasks: Vec<Task>,
    pub(super) changed: Rc<Event>,
}
impl Session {
    pub(super) fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            access: AccessRefresh::Idle,
            devices: BTreeMap::new(),
            tasks: Vec::new(),
            changed: Rc::new(Event::new()),
        }
    }
}
#[derive(Default)]
pub(super) enum AccessRefresh {
    #[default]
    Idle,
    Pending,
    Queued,
}
impl AccessRefresh {
    pub(super) fn request(&mut self) -> bool {
        match self {
            Self::Idle => {
                *self = Self::Pending;
                true
            }
            _ => {
                *self = Self::Queued;
                false
            }
        }
    }
    pub(super) fn finish(&mut self) -> bool {
        matches!(std::mem::take(self), Self::Queued)
    }
}
pub(super) struct Live {
    pub(super) credentials: Option<XiaomiRecord>,
    pub(super) scope: Option<CachedScope>,
    pub(super) members: BTreeSet<String>,
    pub(super) records: BTreeMap<String, DeviceRecord>,
    pub(super) descriptions: BTreeMap<String, CachedPhysicalDevice>,
    pub(super) access: BTreeMap<String, Result<DeviceAccess, crate::xiaomi::central::CentralError>>,
    pub(super) loading: BTreeMap<String, Rc<Lifetime>>,
    pub(super) session: Option<Session>,
}
impl Live {
    pub(super) fn generation(&self) -> Option<u64> {
        self.session.as_ref().map(|s| s.handle.generation())
    }
}
pub(super) struct Context {
    pub(super) live: RefCell<Live>,
    pub(super) registry: DeviceRegistry,
    pub(super) store: Store,
    pub(super) tasks: RefCell<Vec<Task>>,
    pub(super) changed: Event,
    pub(super) status: RefCell<SourceStatus>,
    pub(super) status_changed: Event,
    pub(super) ready: Rc<Ready>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) rejected_token: RefCell<Option<String>>,
    pub(super) refreshing_token: RefCell<Option<String>>,
    pub(super) refreshed_token: RefCell<Option<String>>,
    pub(super) sync_requested: Cell<bool>,
    pub(super) sync_changed: Event,
    pub(super) cloud_paused: Cell<bool>,
    pub(super) fatal: RefCell<Option<SourceError>>,
}
impl Context {
    pub(super) fn session_current(&self, generation: u64) -> bool {
        self.live.borrow().generation() == Some(generation)
    }
    pub(super) fn request_sync(&self) {
        self.sync_requested.set(true);
        self.sync_changed.notify(usize::MAX);
    }
    pub(super) fn set_gateway(&self, status: GatewayStatus) {
        let mut current = self.status.borrow_mut();
        if current.gateway != status {
            log::info!("Xiaomi gateway state: {status:?}");
            current.gateway = status;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn set_catalog_problem(&self, problem: Option<String>) {
        let mut current = self.status.borrow_mut();
        if current.catalog_problem != problem {
            current.catalog_problem = problem;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn problem(&self, did: &str, reason: Option<String>) {
        let name = self
            .live
            .borrow()
            .access
            .get(did)
            .and_then(|access| access.as_ref().ok())
            .and_then(|access| access.name.clone())
            .unwrap_or_else(|| did.into());
        let mut status = self.status.borrow_mut();
        status.pending.retain(|entry| entry.did != did);
        if let Some(reason) = reason {
            status.pending.push(PendingDevice {
                did: did.into(),
                name,
                reason,
            });
            status.pending.sort_by(|a, b| a.did.cmp(&b.did));
        }
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn fail(&self, error: SourceError) {
        *self.fatal.borrow_mut() = Some(error);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn disconnect(&self, reason: String) {
        let session = self.live.borrow_mut().session.take();
        if let Some(session) = session {
            for work in session.devices.values() {
                work.life.close();
            }
            session.handle.close();
        }
        self.live.borrow_mut().access.clear();
        for physical in self.registry.list() {
            physical.set_availability(Availability::Unavailable(reason.clone()));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn add_consumers(
        self: &Rc<Self>,
        metadata: &CachedPhysicalDevice,
        receivers: Vec<CommandReceiver>,
    ) {
        for (function, receiver) in metadata.device.functions.iter().zip(receivers) {
            self.tasks.borrow_mut().push(Box::pin(super::work::consume(
                receiver,
                self.clone(),
                metadata.device.description.key.id.clone(),
                function.id.clone(),
            )));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn install_work(self: &Rc<Self>, did: &str) {
        let mut live = self.live.borrow_mut();
        let Some(record) = live.records.get(did) else {
            return;
        };
        let work = Rc::new(DeviceWork::new(
            record.cached.clone(),
            record.physical.clone(),
        ));
        let Some(session) = live.session.as_mut() else {
            return;
        };
        if session.devices.contains_key(did) {
            return;
        }
        let handle = session.handle.clone();
        let generation = handle.generation();
        session.devices.insert(did.into(), work.clone());
        session.tasks.push(Box::pin(super::work::run(
            work,
            handle,
            self.clone(),
            generation,
        )));
        session.changed.notify(usize::MAX);
    }
    pub(super) fn apply_access(self: &Rc<Self>, entries: Vec<DeviceEntry>) {
        self.live.borrow_mut().access = entries
            .into_iter()
            .map(|entry| (entry.did, entry.access))
            .collect();
        let dids = self
            .live
            .borrow()
            .members
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.reconcile(&did);
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn reconcile(self: &Rc<Self>, did: &str) {
        let (access, cached, existing) = {
            let live = self.live.borrow();
            (
                live.access.get(did).cloned(),
                live.descriptions.get(did).cloned(),
                live.records.contains_key(did),
            )
        };
        let access = match access {
            Some(Ok(access)) => access,
            Some(Err(error)) => {
                self.unavailable_device(did, error.to_string());
                self.problem(
                    did,
                    Some(format!("missing or invalid gateway information: {error}")),
                );
                return;
            }
            None => {
                self.unavailable_device(did, "device is absent from the local gateway".into());
                self.problem(did, Some("waiting for gateway device information".into()));
                return;
            }
        };
        let Some(cached) = cached else {
            self.problem(
                did,
                Some("waiting for a complete device description".into()),
            );
            return;
        };
        let reason = if access.model != cached.model || access.instance_type != cached.instance_type
        {
            Some("local and cloud device descriptions differ".to_owned())
        } else if !access.spec_v2_access || !access.push_available {
            Some("device lacks property or push access".to_owned())
        } else {
            None
        };
        if let Some(reason) = reason {
            self.unavailable_device(did, reason.clone());
            self.problem(did, Some(reason));
            return;
        }
        if existing {
            let changed = self.live.borrow().records[did].cached != cached;
            if changed {
                let label = access.name.as_deref().unwrap_or(did);
                match self.store.devices().publish_cached(&cached, label) {
                    Ok(_) => {
                        self.live.borrow_mut().records.get_mut(did).unwrap().cached =
                            cached.clone();
                        let work = self
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .and_then(|session| session.devices.get(did).cloned());
                        if let Some(work) = work {
                            work.update(cached.clone());
                        }
                    }
                    Err(AdmissionError::Storage(error)) => {
                        self.fail(error.into());
                        return;
                    }
                    Err(error) => {
                        self.block_device(did, error.to_string());
                        return;
                    }
                }
            }
        } else {
            let label = access
                .name
                .as_deref()
                .map(crate::storage::import_label)
                .unwrap_or_else(|| crate::runtime::default_device_label(&cached.model, did));
            let stored = match self.store.devices().publish_cached(&cached, &label) {
                Ok(stored) => stored,
                Err(AdmissionError::Storage(error)) => {
                    self.fail(error.into());
                    return;
                }
                Err(error) => {
                    self.problem(did, Some(error.to_string()));
                    return;
                }
            };
            let mut description = cached.physical_description(stored.label);
            for function in &mut description.functions {
                function.label = stored
                    .functions
                    .iter()
                    .find(|stored| stored.key.id == function.id)
                    .unwrap()
                    .label
                    .clone();
            }
            let (physical, receivers) = PhysicalDevice::new(description);
            self.registry.insert(physical.clone());
            self.live.borrow_mut().records.insert(
                did.into(),
                DeviceRecord {
                    cached: cached.clone(),
                    physical,
                    blocked: None,
                },
            );
            self.add_consumers(&cached, receivers);
            self.install_work(did);
        }
        self.install_work(did);
        let (physical, blocked) = {
            let live = self.live.borrow();
            let record = &live.records[did];
            (record.physical.clone(), record.blocked.clone())
        };
        let diagnostic = self
            .live
            .borrow()
            .session
            .as_ref()
            .and_then(|session| session.devices.get(did))
            .and_then(|work| work.diagnostic());
        self.problem(did, blocked.clone().or(diagnostic));
        let was_available = physical.availability() == Availability::Available;
        let availability = if let Some(reason) = blocked {
            Availability::Unavailable(reason)
        } else if access.online {
            Availability::Available
        } else {
            Availability::Unavailable("device is offline".into())
        };
        let now_available = availability == Availability::Available;
        physical.set_availability(availability);
        let work = self
            .live
            .borrow()
            .session
            .as_ref()
            .and_then(|session| session.devices.get(did).cloned());
        if let Some(work) = work {
            work.retry_subscriptions();
            if now_available && !was_available {
                work.refresh();
            }
        }
    }
    fn unavailable_device(&self, did: &str, reason: String) {
        if let Some(record) = self.live.borrow().records.get(did) {
            record
                .physical
                .set_availability(Availability::Unavailable(reason));
        }
    }
    pub(super) fn block_device(&self, did: &str, reason: String) {
        if let Some(record) = self.live.borrow_mut().records.get_mut(did) {
            record.blocked = Some(reason.clone());
            record
                .physical
                .set_availability(Availability::Unavailable(reason.clone()));
        }
        self.problem(did, Some(reason));
    }
    pub(super) fn remove(self: &Rc<Self>, did: &str) -> Result<(), SourceError> {
        let key = DeviceKey::new("xiaomi", did);
        self.store.devices().remove(&key)?;
        self.registry.remove(&key);
        let handle = {
            let mut live = self.live.borrow_mut();
            live.records.remove(did);
            live.descriptions.remove(did);
            if let Some(life) = live.loading.remove(did) {
                life.close();
            }
            if let Some(session) = live.session.as_mut() {
                if let Some(work) = session.devices.remove(did) {
                    work.life.close();
                }
                Some(session.handle.clone())
            } else {
                None
            }
        };
        self.problem(did, None);
        if let Some(handle) = handle {
            drop(handle.unsubscribe_device(did));
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }
}
pub(super) async fn tasks(context: Rc<Context>) -> Result<(), SourceError> {
    let mut tasks = Vec::<Task>::new();
    loop {
        let listener = context.changed.listen();
        tasks.extend(context.tasks.borrow_mut().drain(..));
        if let Some(error) = context.fatal.borrow_mut().take() {
            return Err(error);
        }
        future::or(
            future::poll_fn(|cx| {
                let mut i = 0;
                while i < tasks.len() {
                    match tasks[i].as_mut().poll(cx) {
                        Poll::Ready(result) => {
                            drop(tasks.swap_remove(i));
                            if let Err(error) = result {
                                return Poll::Ready(Err(error));
                            }
                        }
                        Poll::Pending => i += 1,
                    }
                }
                Poll::Pending
            }),
            async {
                listener.await;
                Ok(())
            },
        )
        .await?;
    }
}
pub(super) fn update_auth(context: &Context, preparation: &BridgePreparation) {
    let mut live = context.live.borrow_mut();
    if let Some(credentials) = &preparation.credentials
        && live
            .scope
            .as_ref()
            .is_none_or(|scope| scope.uid == credentials.uid)
    {
        live.credentials = Some(credentials.clone());
    }
    drop(live);
    context.status.borrow_mut().authentication = preparation.report.clone();
    context.status_changed.notify(usize::MAX);
    context.changed.notify(usize::MAX);
}
pub(super) struct Backoff {
    delay: Duration,
}
impl Backoff {
    pub(super) fn new() -> Self {
        Self {
            delay: Duration::from_secs(1),
        }
    }
    pub(super) fn failed(&mut self, stable: bool) -> Duration {
        if stable {
            self.delay = Duration::from_secs(1);
        }
        let delay = self.delay;
        self.delay = (self.delay * 2).min(MAX_BACKOFF);
        delay
    }
}
