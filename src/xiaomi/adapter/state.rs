use super::{
    bootstrap::Clock,
    cloud::CloudSync,
    model::{AdapterStatus, Ready},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    device::{DeviceRegistry, PhysicalDevice},
    storage::{CachedPhysicalDevice, CachedScope, Store, XiaomiRecord},
    xiaomi::{
        auth::BridgePreparation,
        capability::PropertyBinding,
        central::{CentralError, DeviceAccess, SessionHandle},
        cloud::{CloudClient, CloudDeviceInfo},
    },
};
use event_listener::Event;
use futures_lite::future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
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
    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
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
/// The physical-device lifetime owns consumers; binding work may be replaced independently.
pub(super) struct DeviceRecord {
    pub(super) life: Lifetime,
    pub(super) binding_generation: Cell<u64>,
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
    pub(super) work: Option<Rc<DeviceWork>>,
    pub(super) cloud_access: Option<CloudDeviceInfo>,
    pub(super) cloud_forbidden: bool,
    pub(super) loading: Option<Rc<Lifetime>>,
    pub(super) access: Option<Result<DeviceAccess, CentralError>>,
    pub(super) blocked: Option<String>,
}
impl DeviceSlot {
    pub(super) fn restored(cached: CachedPhysicalDevice, physical: PhysicalDevice) -> Self {
        Self {
            admitted: Some(Rc::new(DeviceRecord {
                life: Lifetime::default(),
                binding_generation: Cell::new(0),
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
                || self.cloud_access.as_ref().is_some_and(|access| {
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
    pub(super) refresh_changed: Rc<Event>,
    pub(super) devices: BTreeMap<String, Rc<DeviceWork>>,
    pub(super) tasks: Rc<TaskSet>,
}
impl Session {
    pub(super) fn new(handle: SessionHandle, refresh_changed: Rc<Event>) -> Self {
        Self {
            handle,
            access: Rc::new(AccessRefresh::default()),
            refresh_changed,
            devices: BTreeMap::new(),
            tasks: Rc::new(TaskSet::default()),
        }
    }
}
#[derive(Default)]
pub(super) struct AccessRefresh {
    full_deadline: Cell<Option<Duration>>,
    pending: RefCell<BTreeSet<String>>,
    changed: Event,
}
pub(super) enum AccessRequest {
    Full,
    Filtered(Vec<String>),
}
impl AccessRefresh {
    pub(super) fn request(&self, now: Duration) {
        if self.full_deadline.get().is_none() {
            self.full_deadline.set(Some(now + Duration::from_secs(3)));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn retry_full(&self, now: Duration) {
        self.full_deadline.set(Some(now + Duration::from_secs(3)));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request_filtered(&self, dids: Vec<String>) {
        self.pending.borrow_mut().extend(dids);
        self.changed.notify(usize::MAX);
    }
    pub(super) async fn next(&self, clock: &dyn Clock) -> AccessRequest {
        loop {
            let listener = self.changed.listen();
            let deadline = self.full_deadline.get();
            if deadline.is_some_and(|deadline| clock.now() >= deadline) {
                self.full_deadline.set(None);
                self.pending.borrow_mut().clear();
                return AccessRequest::Full;
            }
            if !self.pending.borrow().is_empty() {
                return AccessRequest::Filtered(
                    std::mem::take(&mut *self.pending.borrow_mut())
                        .into_iter()
                        .collect(),
                );
            }
            if let Some(deadline) = deadline {
                future::or(listener, clock.wait_until(deadline)).await;
            } else {
                listener.await;
            }
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
    pub(super) cloud: CloudClient,
    pub(super) cloud_sync: RefCell<CloudSync>,
    pub(super) http_paused: Cell<bool>,
    pub(super) cloud_scope_valid: Cell<bool>,
    pub(super) sync_not_before: Cell<Option<Duration>>,
    pub(super) sync_backoff: RefCell<Backoff>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) live: RefCell<Live>,
    pub(super) registry: DeviceRegistry,
    pub(super) store: Store,
    pub(super) tasks: TaskSet,
    pub(super) changed: Event,
    pub(super) status: RefCell<AdapterStatus>,
    pub(super) cloud_problem: RefCell<Option<String>>,
    pub(super) access_problem: RefCell<Option<String>>,
    pub(super) status_changed: Event,
    pub(super) ready: Rc<Ready>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) sync_requested: Cell<bool>,
    pub(super) sync_changed: Event,
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
    maximum: Duration,
}
impl Backoff {
    pub(super) fn new(minimum_secs: u64, maximum_secs: u64) -> Self {
        Self {
            delay: Duration::from_secs(minimum_secs),
            maximum: Duration::from_secs(maximum_secs),
        }
    }
    pub(super) fn failed(&mut self) -> Duration {
        let delay = self.delay;
        self.delay = (self.delay * 2).min(self.maximum);
        delay
    }
}
