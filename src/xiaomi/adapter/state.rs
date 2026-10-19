use super::{
    bootstrap::Clock,
    cloud::CloudSync,
    model::{AdapterStatus, Ready},
    tasks::TaskSet,
    work::DeviceWork,
};
use crate::{
    configuration::{DerivedConfiguration, NativeDescription, Scope},
    device::{DeviceRegistry, PhysicalDevice},
    storage::{Store, XiaomiRecord},
    xiaomi::{
        auth::{AuthenticationState, CertificateUpdate},
        central::{CentralError, DeviceAccess, SessionHandle},
        certificate::CertificateValidity,
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
    /// The single latest valid native description. It may be newer than
    /// the installed runtime (pending commit) and survives a failed commit so
    /// the next plan can retry it. It is never replaced by the runtime's older
    /// committed metadata.
    pub(super) latest: Option<NativeDescription>,
    /// Directory name for `latest`; display/planning metadata only.
    pub(super) latest_name: Option<String>,

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
    pub(super) fn restored(work: Rc<DeviceWork>) -> Self {
        Self {
            latest: Some(work.native.clone()),
            admitted: Some(work),
            ..Self::default()
        }
    }
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
        let Some(latest) = &self.latest else {
            return true;
        };
        self.cloud_access.as_ref().is_some_and(|access| {
            access.model != latest.model || access.instance_type != latest.instance_type
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
    pub(super) scope: Option<Scope>,
    pub(super) devices: BTreeMap<String, DeviceSlot>,
    pub(super) session: Option<Session>,
}
pub(super) struct Context {
    pub(super) commands: Rc<super::commands::Commands>,
    pub(super) light_writes: super::commands::LightWriteGates,
    pub(super) relations: RefCell<super::relations::Relations>,
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
