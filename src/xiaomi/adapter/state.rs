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
        auth::{AuthenticationState, CertificateUpdate},
        capability::PropertyBinding,
        central::{CentralError, DeviceAccess, SessionHandle},
        certificate::CertificateValidity,
        cloud::{CloudClient, CloudDeviceInfo, CloudGroup},
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
/// Cloud relation metadata for one group identity in the selected home.
#[derive(Clone)]
pub(super) struct GroupMeta {
    pub(super) model: String,
    /// The relation confirmed by the current directory read. `None` while a
    /// read is pending or after a failed read, so only the last published
    /// relation stays authoritative instead of a stale candidate relation.
    pub(super) relation: Option<CloudGroup>,
}
/// One entry owns all preparation and admission state for a current home member.
#[derive(Default)]
pub(super) struct DeviceSlot {
    /// A description awaiting admission; accepted descriptions live only in the record.
    pub(super) candidate: Option<(CachedPhysicalDevice, Option<String>)>,
    pub(super) admitted: Option<Rc<DeviceRecord>>,
    pub(super) work: Option<Rc<DeviceWork>>,
    pub(super) cloud_access: Option<CloudDeviceInfo>,
    pub(super) cloud_forbidden: bool,
    pub(super) loading: Option<Rc<Lifetime>>,
    pub(super) access: Option<Result<DeviceAccess, CentralError>>,
    pub(super) blocked: Option<String>,
    /// The block is a confirmed unsupported capability, not a temporary failure.
    pub(super) blocked_unsupported: bool,
    /// Latest cloud relation metadata when this member is a known group identity.
    pub(super) group: Option<GroupMeta>,
    pub(super) last_switch_reason: Option<String>,
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
            self.cloud_access.as_ref().is_some_and(|access| {
                access.model != cached.model || access.instance_type != cached.instance_type
            })
        };
        match &self.admitted {
            Some(record) => self.blocked.is_some() || differs(&record.cached.borrow()),
            None => self
                .candidate
                .as_ref()
                .is_none_or(|(cached, _)| differs(cached)),
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
    full_deadline: Cell<Option<Duration>>,
    pending: RefCell<BTreeSet<String>>,
    changed: Event,
    recovery: Cell<Recovery>,
}
#[derive(Clone, Copy, Default)]
enum Recovery {
    #[default]
    Healthy,
    Scheduled(Duration),
    Running,
}
pub(super) enum AccessRequest {
    Full,
    Filtered(Vec<String>),
    Recovery,
}
impl AccessRefresh {
    pub(super) fn recovery_status(&self, clock: &dyn Clock) -> super::model::RecoveryStatus {
        use super::model::RecoveryStatus;
        match self.recovery.get() {
            Recovery::Healthy => RecoveryStatus::None,
            Recovery::Running => RecoveryStatus::Running,
            Recovery::Scheduled(due) => {
                let remaining = due.saturating_sub(clock.now());
                let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
                RecoveryStatus::Scheduled(clock.unix_now() + seconds as i64)
            }
        }
    }
    pub(super) fn is_degraded(&self) -> bool {
        !matches!(self.recovery.get(), Recovery::Healthy)
    }
    pub(super) fn degrade(&self, now: Duration) -> bool {
        if self.is_degraded() {
            return false;
        }
        self.full_deadline.set(None);
        self.pending.borrow_mut().clear();
        self.recovery
            .set(Recovery::Scheduled(now + Duration::from_secs(60)));
        self.changed.notify(usize::MAX);
        true
    }
    pub(super) fn recovery_finished(&self, now: Duration) {
        if matches!(self.recovery.get(), Recovery::Running) {
            self.recovery
                .set(Recovery::Scheduled(now + Duration::from_secs(300)));
            self.changed.notify(usize::MAX);
        }
    }
    pub(super) fn recovered(&self) {
        self.recovery.set(Recovery::Healthy);
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request_initial(&self, now: Duration) {
        self.full_deadline.set(Some(now));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request(&self, now: Duration) {
        if self.is_degraded() {
            return;
        }
        if self.full_deadline.get().is_none() {
            self.full_deadline.set(Some(now + Duration::from_secs(3)));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn retry_full(&self, now: Duration) {
        if self.is_degraded() {
            return;
        }
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
            let recovery_due = match self.recovery.get() {
                Recovery::Scheduled(due) => Some(due),
                _ => None,
            };
            if recovery_due.is_some_and(|due| clock.now() >= due) {
                self.recovery.set(Recovery::Running);
                return AccessRequest::Recovery;
            }
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
            let deadline = deadline.into_iter().chain(recovery_due).min();
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
    /// The runtime map covers the whole current-home membership from a
    /// complete directory read; before that, absence is not deletion evidence.
    pub(super) complete: bool,
    pub(super) devices: BTreeMap<String, DeviceSlot>,
    pub(super) session: Option<Session>,
}
pub(super) struct Context {
    pub(super) commands: Rc<super::commands::Commands>,
    pub(super) light_writes: super::commands::LightWriteGates,
    pub(super) notifications: RefCell<Option<Rc<super::notifications::CloudSession>>>,
    pub(super) credentials_changed: Event,
    #[cfg(test)]
    pub(super) notification_connector: Cell<super::notifications::NotificationConnector>,
    pub(super) priority: Cell<super::routing::PathPriority>,
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
impl Context {
    pub(super) fn adopt_credentials(&self, credentials: XiaomiRecord) {
        let mut live = self.live.borrow_mut();
        let changed = live.credentials.as_ref().is_none_or(|old| {
            old.tokens != credentials.tokens
                || old.certificate_pem != credentials.certificate_pem
                || old.oauth_client_uuid != credentials.oauth_client_uuid
        });
        live.credentials = Some(credentials);
        drop(live);
        if changed {
            self.credentials_changed.notify(usize::MAX);
            self.changed.notify(usize::MAX);
            self.refresh_changed.notify(usize::MAX);
            self.update_all_availability();
            self.update_routing("cloud credentials updated");
        }
    }
    pub(super) fn authentication(&self, authentication: AuthenticationState) {
        let mut status = self.status.borrow_mut();
        status.authentication.authentication = authentication;
        status
            .authentication
            .set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn certificate(&self, validity: CertificateValidity, update: CertificateUpdate) {
        let mut status = self.status.borrow_mut();
        status.authentication.certificate = Some(validity);
        status.authentication.certificate_update = update;
        status
            .authentication
            .set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
        self.update_all_availability();
        self.update_routing("local gateway certificate updated");
        self.changed.notify(usize::MAX);
    }
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
