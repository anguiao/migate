use super::{
    bootstrap::Clock,
    model::{GatewayStatus, PendingDevice, Ready, SourceError, SourceStatus},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    device::{Availability, CommandReceiver, DeviceKey, DeviceRegistry, PhysicalDevice},
    storage::{AdmissionError, CachedPhysicalDevice, CachedScope, Store, XiaomiRecord},
    xiaomi::{
        auth::BridgePreparation,
        capability::PropertyBinding,
        central::{CentralError, DeviceAccess, DeviceEntry, SessionHandle},
    },
};
use event_listener::Event;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(30);
#[derive(Default)]
pub(super) struct Lifetime {
    closed: Cell<bool>,
    changed: Event,
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
/// The accepted binding and physical state are shared by commands and session work.
pub(super) struct DeviceRecord {
    pub(super) cached: RefCell<CachedPhysicalDevice>,
    pub(super) physical: PhysicalDevice,
}
impl DeviceRecord {
    pub(super) fn binding(&self, id: &str) -> PropertyBinding {
        self.cached
            .borrow()
            .device
            .functions
            .iter()
            .find(|function| function.description.id == id)
            .unwrap()
            .power
    }
}
/// One entry owns all preparation and admission state for a current home member.
#[derive(Default)]
pub(super) struct DeviceSlot {
    /// A description awaiting admission; accepted descriptions live only in the record.
    pub(super) candidate: Option<CachedPhysicalDevice>,
    pub(super) admitted: Option<Rc<DeviceRecord>>,
    pub(super) loading: Option<Rc<Lifetime>>,
    pub(super) access: Option<Result<DeviceAccess, CentralError>>,
    pub(super) blocked: Option<String>,
}
impl DeviceSlot {
    pub(super) fn restored(cached: CachedPhysicalDevice, physical: PhysicalDevice) -> Self {
        Self {
            admitted: Some(Rc::new(DeviceRecord {
                cached: RefCell::new(cached),
                physical,
            })),
            ..Self::default()
        }
    }
    pub(super) fn needs_description(&self) -> bool {
        if self.loading.is_some() {
            return false;
        }
        let differs = |cached: &CachedPhysicalDevice| {
            self.access
                .as_ref()
                .and_then(|access| access.as_ref().ok())
                .is_some_and(|access| {
                    access.model != cached.model || access.instance_type != cached.instance_type
                })
        };
        match &self.admitted {
            Some(record) => self.blocked.is_some() || differs(&record.cached.borrow()),
            None => self.candidate.as_ref().is_none_or(differs),
        }
    }
}
pub(super) struct Session {
    pub(super) handle: SessionHandle,
    pub(super) access: Rc<AccessRefresh>,
    pub(super) devices: BTreeMap<String, Rc<DeviceWork>>,
    pub(super) tasks: Rc<TaskSet>,
}
impl Session {
    pub(super) fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            access: Rc::new(AccessRefresh::default()),
            devices: BTreeMap::new(),
            tasks: Rc::new(TaskSet::default()),
        }
    }
}
#[derive(Default)]
pub(super) struct AccessRefresh {
    pub(super) requested: Cell<bool>,
    changed: Event,
}
impl AccessRefresh {
    pub(super) fn request(&self) {
        self.requested.set(true);
        self.changed.notify(usize::MAX);
    }
    pub(super) async fn next(&self) {
        loop {
            let listener = self.changed.listen();
            if self.requested.replace(false) {
                return;
            }
            listener.await;
        }
    }
}
pub(super) struct Live {
    pub(super) credentials: Option<XiaomiRecord>,
    pub(super) scope: Option<CachedScope>,
    pub(super) devices: BTreeMap<String, DeviceSlot>,
    pub(super) session: Option<Session>,
}
pub(super) struct Context {
    pub(super) live: RefCell<Live>,
    pub(super) registry: DeviceRegistry,
    pub(super) store: Store,
    pub(super) tasks: TaskSet,
    pub(super) changed: Event,
    pub(super) status: RefCell<SourceStatus>,
    pub(super) cloud_problem: RefCell<Option<String>>,
    pub(super) access_problem: RefCell<Option<String>>,
    pub(super) status_changed: Event,
    pub(super) ready: Rc<Ready>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) sync_requested: Cell<bool>,
    pub(super) sync_changed: Event,
}
impl Context {
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
        *self.cloud_problem.borrow_mut() = problem;
        self.update_catalog_problem();
    }
    pub(super) fn set_access_problem(&self, problem: Option<String>) {
        *self.access_problem.borrow_mut() = problem;
        self.update_catalog_problem();
    }
    fn update_catalog_problem(&self) {
        let problems = [
            self.cloud_problem.borrow().clone(),
            self.access_problem.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let problem = (!problems.is_empty()).then(|| problems.join("; "));
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
            .devices
            .get(did)
            .and_then(|slot| slot.access.as_ref())
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
    pub(super) fn disconnect(&self, reason: String) {
        let session = self.live.borrow_mut().session.take();
        if let Some(session) = session {
            for work in session.devices.values() {
                work.life.close();
            }
            session.handle.close();
        }
        for slot in self.live.borrow_mut().devices.values_mut() {
            slot.access = None;
        }
        self.set_access_problem(None);
        for physical in self.registry.list() {
            physical.set_availability(Availability::Unavailable(reason.clone()));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn add_consumers(
        self: &Rc<Self>,
        record: Rc<DeviceRecord>,
        receivers: Vec<CommandReceiver>,
    ) {
        for (function, receiver) in record.physical.functions().iter().zip(receivers) {
            self.tasks.spawn(super::work::consume(
                receiver,
                self.clone(),
                record.clone(),
                function.function_key().id.clone(),
            ));
        }
    }
    fn install_work(self: &Rc<Self>, did: &str) {
        let mut live = self.live.borrow_mut();
        let record = live.devices[did].admitted.as_ref().unwrap().clone();
        let Some(session) = live.session.as_mut() else {
            return;
        };
        if session.devices.contains_key(did) {
            return;
        }
        let work = Rc::new(DeviceWork::new(record));
        let handle = session.handle.clone();
        session.devices.insert(did.into(), work.clone());
        session
            .tasks
            .spawn(super::work::run(work, handle, self.clone()));
    }
    /// Apply only observations in wire order; admission and storage run after the request future wakes.
    pub(super) fn observe_access(&self, entries: Vec<DeviceEntry>) {
        let mut entries = entries
            .into_iter()
            .map(|entry| (entry.did, entry.access))
            .collect::<BTreeMap<_, _>>();
        let dids = {
            let mut live = self.live.borrow_mut();
            for (did, slot) in &mut live.devices {
                slot.access = entries.remove(did);
            }
            live.devices.keys().cloned().collect::<Vec<_>>()
        };
        for did in dids {
            self.update_availability(&did);
        }
    }
    pub(super) fn reconcile_all(self: &Rc<Self>) -> Result<(), SourceError> {
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.reconcile(&did)?;
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }
    pub(super) fn reconcile(self: &Rc<Self>, did: &str) -> Result<(), SourceError> {
        let (access, cached, admitted) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (
                slot.access.clone(),
                slot.candidate.clone().or_else(|| {
                    slot.admitted
                        .as_ref()
                        .map(|record| record.cached.borrow().clone())
                }),
                slot.admitted.clone(),
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
                return Ok(());
            }
            None => {
                self.unavailable_device(did, "device is absent from the local gateway".into());
                self.problem(did, Some("waiting for gateway device information".into()));
                return Ok(());
            }
        };
        let Some(cached) = cached else {
            self.problem(
                did,
                Some("waiting for a complete device description".into()),
            );
            return Ok(());
        };
        if let Some(reason) = access_problem(&access, &cached) {
            self.unavailable_device(did, reason.clone());
            self.problem(did, Some(reason));
            return Ok(());
        }
        if let Some(record) = admitted {
            if *record.cached.borrow() != cached {
                match self
                    .store
                    .devices()
                    .publish_cached(&cached, access.name.as_deref().unwrap_or(did))
                {
                    Ok(_) => {
                        *record.cached.borrow_mut() = cached;
                        if let Some(work) = self
                            .live
                            .borrow()
                            .session
                            .as_ref()
                            .and_then(|session| session.devices.get(did))
                        {
                            work.bindings_changed();
                        }
                    }
                    Err(AdmissionError::Storage(error)) => return Err(error.into()),
                    Err(error) => {
                        self.block_device(did, error.to_string());
                        return Ok(());
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
                Err(AdmissionError::Storage(error)) => return Err(error.into()),
                Err(error) => {
                    self.problem(did, Some(error.to_string()));
                    return Ok(());
                }
            };
            let (physical, receivers) = PhysicalDevice::new(stored.description());
            self.registry.insert(physical.clone());
            let record = Rc::new(DeviceRecord {
                cached: RefCell::new(cached),
                physical,
            });
            self.live
                .borrow_mut()
                .devices
                .get_mut(did)
                .unwrap()
                .admitted = Some(record.clone());
            self.add_consumers(record, receivers);
        }
        self.live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .candidate = None;
        self.install_work(did);
        self.update_availability(did);
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let diagnostic = live
            .session
            .as_ref()
            .and_then(|session| session.devices.get(did))
            .and_then(|work| work.diagnostic());
        let problem = slot.blocked.clone().or(diagnostic);
        drop(live);
        self.problem(did, problem);
        if let Some(work) = self
            .live
            .borrow()
            .session
            .as_ref()
            .and_then(|session| session.devices.get(did))
        {
            work.retry_subscriptions();
        }
        Ok(())
    }
    fn update_availability(&self, did: &str) {
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let Some(record) = &slot.admitted else { return };
        let reason = slot.blocked.clone().or_else(|| match &slot.access {
            Some(Ok(access)) => access_problem(access, &record.cached.borrow())
                .or_else(|| (!access.online).then(|| "device is offline".into())),
            Some(Err(error)) => Some(error.to_string()),
            None => Some("device is absent from the local gateway".into()),
        });
        let was_available = record.physical.availability() == Availability::Available;
        let availability = reason.map_or(Availability::Available, Availability::Unavailable);
        let now_available = availability == Availability::Available;
        record.physical.set_availability(availability);
        if now_available
            && !was_available
            && let Some(work) = live
                .session
                .as_ref()
                .and_then(|session| session.devices.get(did))
        {
            work.refresh();
        }
    }
    fn unavailable_device(&self, did: &str, reason: String) {
        if let Some(record) = &self.live.borrow().devices[did].admitted {
            record
                .physical
                .set_availability(Availability::Unavailable(reason));
        }
    }
    pub(super) fn block_device(&self, did: &str, reason: String) {
        self.live.borrow_mut().devices.get_mut(did).unwrap().blocked = Some(reason.clone());
        self.unavailable_device(did, reason.clone());
        self.problem(did, Some(reason));
    }
    pub(super) fn remove(self: &Rc<Self>, did: &str) -> Result<(), SourceError> {
        let key = DeviceKey::new("xiaomi", did);
        self.store.devices().remove(&key)?;
        self.registry.remove(&key);
        let handle = {
            let mut live = self.live.borrow_mut();
            if let Some(life) = live.devices.remove(did).unwrap().loading {
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
fn access_problem(access: &DeviceAccess, cached: &CachedPhysicalDevice) -> Option<String> {
    if access.model != cached.model || access.instance_type != cached.instance_type {
        Some("local and cloud device descriptions differ".into())
    } else if !access.spec_v2_access || !access.push_available {
        Some("device lacks property or push access".into())
    } else {
        None
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
