use super::{
    bootstrap::Clock,
    credentials::HaSync,
    model::{AdapterStatus, Ready},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::xiaomi::capability::SourceDescription;
#[cfg(test)]
use crate::xiaomi::cloud::CloudClient;
use crate::{
    configuration::{DerivedConfiguration, Scope},
    device::{DeviceRegistry, PhysicalDevice},
    storage::{Store, XiaomiRecord},
    xiaomi::{
        auth::{AuthenticationState, CertificateUpdate, FailureReason},
        central::{CentralError, DeviceAccess, RequestTimeout, SessionHandle},
        certificate::CertificateValidity,
        mijia::CloudDeviceInfo,
        mijia::MijiaClient,
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
/// The current logical delivery target of one native runtime. It owns the
/// physical device and the command-consumer lifetime; replacing it retires
/// only logical delivery, never the native control or its subscriptions.
pub(super) struct LogicalSink {
    pub(super) physical: PhysicalDevice,
    pub(super) life: Lifetime,
}
impl LogicalSink {
    pub(super) fn new(physical: PhysicalDevice) -> Self {
        Self {
            physical,
            life: Lifetime::default(),
        }
    }
    pub(super) fn close(&self) {
        self.physical.close();
        self.life.close();
    }
}
/// One entry owns all preparation and admission state for a current home member.
#[derive(Default)]
pub(super) struct DeviceSlot {
    /// This slot is a native group source. Executable group rules live only in
    /// the installed committed graph.
    pub(super) group: bool,
    pub(super) lan: super::lan::LanSlot,
    /// Latest complete source fact, retained while its commit is pending.
    pub(super) candidate: Option<SourceDescription>,
    /// Source fact paired with the installed configuration.
    pub(super) committed: Option<SourceDescription>,
    /// Startup recognition failed; unchanged directory facts cannot clear its
    /// diagnostic. Only a newly recognized description resolves it.
    pub(super) recompute_failed: bool,
    /// A confirmed group record lacked the target instance type.
    pub(super) description_target_missing: bool,
    pub(super) confirmed: bool,
    /// The installed native runtime; native control and subscriptions live here.
    pub(super) admitted: Option<Rc<DeviceWork>>,
    pub(super) cloud_access: Option<CloudDeviceInfo>,
    pub(super) cloud_forbidden: bool,
    /// The one in-flight directory load and the model/URN target it may
    /// apply. `None` target is a genuine first read without metadata facts.
    pub(super) loading: Option<Loading>,
    pub(super) access: Option<Result<DeviceAccess, CentralError>>,
    pub(super) blocked: Option<String>,
    pub(super) last_switch_reason: Option<String>,
}
#[derive(Clone)]
pub(super) struct Loading {
    pub(super) life: Rc<Lifetime>,
    pub(super) target: Option<(String, String)>,
}
impl DeviceSlot {
    /// Retire a load aimed at an older target; online changes keep it.
    pub(super) fn retarget_loading(&mut self, target: &Option<(String, String)>) {
        if self
            .loading
            .as_ref()
            .is_some_and(|loading| &loading.target != target)
            && let Some(loading) = self.loading.take()
        {
            loading.life.close();
        }
    }
    pub(super) fn start_loading(&mut self, target: Option<(String, String)>) -> Rc<Lifetime> {
        debug_assert!(self.loading.is_none());
        let life = Rc::new(Lifetime::default());
        self.loading = Some(Loading {
            life: life.clone(),
            target,
        });
        life
    }
    pub(super) fn loading_is(&self, life: &Rc<Lifetime>) -> bool {
        self.loading
            .as_ref()
            .is_some_and(|loading| Rc::ptr_eq(&loading.life, life))
    }
    /// Finish this exact load once; a superseded load is left untouched.
    pub(super) fn finish_loading(&mut self, life: &Rc<Lifetime>) -> bool {
        if self.loading_is(life) {
            self.loading = None;
            true
        } else {
            false
        }
    }
    /// A directory description read is needed only when the latest valid
    /// fact is missing or its metadata differs from the current access facts.
    pub(super) fn needs_description(&self) -> bool {
        if self.loading.is_some() {
            return false;
        }
        let Some(source) = &self.candidate else {
            return true;
        };
        self.cloud_access.as_ref().is_some_and(|access| {
            access.model != source.raw.model || access.instance_type != source.raw.instance_type
        })
    }
}
pub(super) struct Session {
    pub(super) handle: SessionHandle,
    pub(super) access: Rc<AccessRefresh>,
    pub(super) tasks: Rc<TaskSet>,
}
impl Session {
    pub(super) fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            access: Rc::new(AccessRefresh::default()),
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
    pub(super) fn request_initial(&self, now: Duration) {
        self.full_deadline.set(Some(now));
        self.changed.notify(usize::MAX);
    }
    pub(super) fn request(&self, now: Duration) {
        if self.full_deadline.get().is_none() {
            self.full_deadline.set(Some(now + Duration::from_secs(3)));
        }
        self.changed.notify(usize::MAX);
    }
    pub(super) fn retry_full(&self, now: Duration, timeout: Option<RequestTimeout>) {
        // A stalled directory stays low-frequency without degrading the hub session.
        let delay = if timeout.is_some() { 60 } else { 3 };
        self.full_deadline
            .set(Some(now + Duration::from_secs(delay)));
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
    pub(super) mijia_generation: Option<u64>,
    pub(super) ha_generation: Option<u64>,
    pub(super) scope: Option<Scope>,
    pub(super) devices: BTreeMap<String, DeviceSlot>,
    pub(super) session: Option<Session>,
}
pub(super) struct Context {
    pub(super) commands: Rc<super::commands::Commands>,
    pub(super) write_gates: super::commands::DeviceWriteGates,
    pub(super) relations: RefCell<super::relations::Relations>,
    pub(super) notifications: RefCell<Option<Rc<super::notifications::CloudSession>>>,
    #[cfg(test)]
    pub(super) mqtt_retry_deadline: Cell<Option<Duration>>,
    pub(super) credentials_changed: Event,
    #[cfg(test)]
    pub(super) notification_connector: Cell<super::notifications::NotificationConnector>,
    pub(super) priority: Cell<super::routing::PathPriority>,
    pub(super) mijia: MijiaClient,
    #[cfg(test)]
    pub(super) cloud: CloudClient,
    pub(super) lan_discovery: crate::xiaomi::lan::Discovery,
    pub(super) lan_scope_invalid: Cell<bool>,
    pub(super) lan_network_changed: Event,
    #[cfg(test)]
    pub(super) lan_port: Cell<u16>,
    #[cfg(test)]
    pub(super) lan_test_endpoints: RefCell<BTreeMap<std::net::Ipv4Addr, std::net::SocketAddr>>,
    #[cfg(test)]
    pub(super) lan_timeout: Cell<Duration>,
    pub(super) ha_sync: RefCell<HaSync>,
    pub(super) ha_changed: Event,
    pub(super) mijia_rejected: RefCell<Option<u64>>,
    pub(super) mijia_changed: Event,
    pub(super) http_paused: Cell<bool>,
    pub(super) cloud_scope_valid: Cell<bool>,
    pub(super) sync_not_before: Cell<Option<Duration>>,
    pub(super) sync_backoff: RefCell<Backoff>,
    pub(super) refresh_changed: Rc<Event>,
    pub(super) initial_reads_changed: Rc<Event>,
    pub(super) live: RefCell<Live>,
    /// The one committed group graph installed with the serving configuration.
    pub(super) graph: RefCell<Rc<DerivedConfiguration>>,
    pub(super) registry: DeviceRegistry,
    pub(super) store: Store,
    pub(super) tasks: TaskSet,
    pub(super) configuration_changed: Event,
    #[cfg(test)]
    pub(super) snapshot_observer: RefCell<Option<Box<dyn FnMut()>>>,
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
        let snapshot = self.store.xiaomi().current_snapshot();
        let generation = snapshot.mijia_generation();
        let ha_generation = snapshot.ha_generation();
        let recovery_stop = credentials.recovery.stop;
        let ha_stopped = credentials.ha.stopped;
        let mut live = self.live.borrow_mut();
        let generation_changed = live.mijia_generation != generation;
        let ha_generation_changed = live.ha_generation != ha_generation;
        let changed = live.credentials.as_ref() != Some(&credentials)
            || generation_changed
            || ha_generation_changed;
        live.credentials = Some(credentials);
        live.mijia_generation = generation;
        live.ha_generation = ha_generation;
        drop(live);
        {
            let mut status = self.status.borrow_mut();
            let status_changed = status.auth.recovery_stop != recovery_stop || generation_changed;
            status.auth.recovery_stop = recovery_stop;
            if generation_changed {
                status.auth.mijia = AuthenticationState::Checking;
            }
            if status_changed {
                status.auth.set_completed_at(self.clock.unix_now());
                self.status_changed.notify(usize::MAX);
            }
        }
        if generation_changed {
            *self.mijia_rejected.borrow_mut() = None;
            self.http_paused.set(false);
            self.sync_not_before.set(None);
            *self.sync_backoff.borrow_mut() = Backoff::new(1, 30);
            self.mijia_changed.notify(usize::MAX);
            self.request_sync();
        }
        if ha_generation_changed {
            *self.ha_sync.borrow_mut() = if ha_stopped {
                HaSync::Paused
            } else {
                HaSync::Idle
            };
            let authentication = if self.live.borrow().ha_generation.is_some() {
                AuthenticationState::Checking
            } else {
                AuthenticationState::SignInRequired(FailureReason::HaAuthorizationStopped)
            };
            self.ha_authentication(authentication);
            self.ha_changed.notify(usize::MAX);
        }
        if changed {
            self.credentials_changed.notify(usize::MAX);
            self.changed.notify(usize::MAX);
            self.refresh_changed.notify(usize::MAX);
            self.update_all_availability();
            self.update_routing("cloud credentials updated");
        }
    }
    pub(super) fn reload_credentials(&self) {
        let record = self
            .store
            .xiaomi()
            .current_snapshot()
            .record
            .expect("active account");
        self.adopt_credentials(record);
    }
    pub(super) fn mijia_authentication(&self, authentication: AuthenticationState) {
        let mut status = self.status.borrow_mut();
        status.auth.mijia = authentication;
        status.auth.set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn confirm_ha(&self) {
        let checking = matches!(self.status.borrow().auth.ha, AuthenticationState::Checking);
        if checking {
            self.ha_authentication(AuthenticationState::Authenticated);
        }
    }
    pub(super) fn ha_authentication(&self, authentication: AuthenticationState) {
        let mut status = self.status.borrow_mut();
        status.auth.ha = authentication;
        status.auth.set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
    }
    pub(super) fn certificate(&self, validity: CertificateValidity, update: CertificateUpdate) {
        let mut status = self.status.borrow_mut();
        status.auth.certificate = Some(validity);
        status.auth.certificate_update = update;
        status.auth.set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
        self.update_all_availability();
        self.update_routing("hub certificate updated");
        self.changed.notify(usize::MAX);
    }

    pub(super) fn certificate_unavailable(&self, update: CertificateUpdate) {
        let mut status = self.status.borrow_mut();
        if status.auth.certificate.is_none() && status.auth.certificate_update == update {
            return;
        }
        status.auth.certificate = None;
        status.auth.certificate_update = update;
        status.auth.set_completed_at(self.clock.unix_now());
        drop(status);
        self.status_changed.notify(usize::MAX);
        self.update_all_availability();
        self.update_routing("hub certificate unavailable");
        self.refresh_changed.notify(usize::MAX);
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
