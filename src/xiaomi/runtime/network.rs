use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    time::{Duration, Instant},
};

use async_io::Timer;
use event_listener::Event;
use futures_lite::future;
use futures_util::{FutureExt, future::LocalBoxFuture, future::select_all};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::{
    device::{DeviceService, PhysicalDeviceId},
    storage::{
        AuthSnapshot, SessionCheckFailure, StorageError, Store, XiaomiAuthObservation,
        XiaomiAuthObserver, XiaomiStore,
    },
    xiaomi::{
        auth::{AuthError, AuthReport, AuthService},
        catalog::{assemble_catalog, persist_catalog},
        certificate::XIAOMI_CA_PEM,
        cloud::{CLOUD_MQTT_HOST, CLOUD_MQTT_PORT, CloudClient, CloudError, CloudNotification},
        discovery::{
            DiscoveryBrowser, DiscoveryRegistry, GatewayCandidate, GatewayEndpoint, MdnsEvent,
            NetworkEpoch, NetworkMonitor, NetworkUpdate,
        },
        gateway::{EventArguments, GatewayNotification},
        lan::{LanEventArguments, LanNotification, LanProperty, LanTarget},
        mqtt::{CloudTlsConfig, GatewayTlsConfig, MqttConfig},
    },
};

use super::{
    AdmissionController, AuthenticatedGateway, AuthenticatedLan, CloudEvidence,
    CloudNotificationStartup, CloudStatus, CommandRuntime, CurrentSessionRegistry, GatewayStartup,
    LanOperationEvidence, LegacyOperationEvidence, PushSource, RuntimeTransports, SessionAuthority,
    StateRuntime, SubscriptionToken, TransportStartupError, start_cloud_notifications,
    start_gateway, start_lan,
};

const AUTH_OBSERVE_INTERVAL: Duration = Duration::from_millis(100);
const AUTH_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);
const NETWORK_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(600);
const ROUTE_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_SESSION_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CLOUD_NOTIFICATION_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CONNECTION_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
const LAN_SUBSCRIPTION_RENEWAL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub enum XiaomiRuntimeError {
    Storage(StorageError),
    Cloud(CloudError),
    Auth(AuthError),
}

#[derive(Clone, Debug)]
pub struct XiaomiRuntimeStatus {
    pub admission: super::AdmissionSnapshot,
    pub candidates: Vec<XiaomiCandidateStatus>,
    pub gateways: Vec<XiaomiGatewayStatus>,
    pub diagnostics: Vec<XiaomiRuntimeDiagnostic>,
}

#[derive(Clone, Debug)]
pub enum XiaomiRuntimeDiagnostic {
    Command(super::RuntimeDiagnostic),
    State(super::StateDiagnostic),
    Boundary(XiaomiBoundaryDiagnostic),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiBoundaryDiagnostic {
    pub component: XiaomiRuntimeComponent,
    pub stage: XiaomiFailureStage,
    pub code: XiaomiSafeFailureCode,
    pub subject: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiRuntimeComponent {
    Network,
    Catalog,
    Gateway,
    Lan,
    Cloud,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiFailureStage {
    Discover,
    Connect,
    Authenticate,
    Subscribe,
    Read,
    Write,
    Publish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiSafeFailureCode {
    Unavailable,
    Timeout,
    Unauthorized,
    Protocol,
    Rejected(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiCandidateStatus {
    pub account_id: String,
    pub home_id: String,
    pub did: String,
    pub parent_did: String,
    pub name: String,
    pub model: String,
    pub state: XiaomiCandidateState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiCandidateState {
    Unsupported,
    Unrecognized,
    WaitingForGateway,
    Ready,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiGatewayStatus {
    pub did: u64,
    pub home_group: String,
    pub unverified: bool,
    pub authenticated: bool,
}

impl fmt::Display for XiaomiRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Cloud(error) => error.fmt(formatter),
            Self::Auth(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for XiaomiRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
            Self::Auth(error) => Some(error),
        }
    }
}

impl From<StorageError> for XiaomiRuntimeError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<CloudError> for XiaomiRuntimeError {
    fn from(error: CloudError) -> Self {
        Self::Cloud(error)
    }
}

impl From<AuthError> for XiaomiRuntimeError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

#[derive(Clone)]
pub struct XiaomiRuntime {
    inner: Rc<Inner>,
}

struct Inner {
    service: DeviceService,
    auth_report: RefCell<AuthReport>,
    status: RefCell<XiaomiRuntimeStatus>,
    diagnostics: RefCell<VecDeque<XiaomiRuntimeDiagnostic>>,
    failure: RefCell<Option<StorageError>>,
    failed: Event,
    refresh_requested: Rc<Cell<bool>>,
    wake: Rc<Event>,
    running: Cell<bool>,
    stopped: Cell<bool>,
    runner: RefCell<Option<Runner>>,
    commands: CommandRuntime,
    state: StateRuntime,
    registry: CurrentSessionRegistry,
    xiaomi: XiaomiStore,
    devices: crate::storage::DeviceStore,
}

struct RuntimeRunGuard {
    inner: Rc<Inner>,
}

impl Drop for RuntimeRunGuard {
    fn drop(&mut self) {
        self.inner.stopped.set(true);
        self.inner.registry.revoke_all();
        self.inner.commands.stop();
        self.inner.state.stop();
        self.inner.running.set(false);
        self.inner.wake.notify(usize::MAX);
    }
}

struct Runner {
    auth: Rc<AuthService>,
    auth_task: Option<LocalBoxFuture<'static, AuthTaskResult>>,
    observer: XiaomiAuthObserver,
    admission: AdmissionController,
    network: Option<NetworkMonitor>,
    network_task: Option<LocalBoxFuture<'static, NetworkTaskResult>>,
    cloud: Rc<CloudClient>,
    catalog_task: Option<LocalBoxFuture<'static, CatalogTaskResult>>,
    catalog: Option<super::AdmissionCatalog>,
    network_update: Option<NetworkUpdate>,
    discovery: Option<DiscoveryRegistry>,
    browser_read: Option<LocalBoxFuture<'static, BrowserResult>>,
    next_browser_start: Instant,
    connecting_gateways: BTreeMap<u64, ConnectingGateway>,
    gateway_setup: LanSetupSlots,
    gateways: BTreeMap<u64, ActiveGateway>,
    gateway_routes: BTreeMap<PhysicalDeviceId, u64>,
    lans: BTreeMap<PhysicalDeviceId, ActiveLan>,
    lan_setup: LanSetupSlots,
    cloud_notifications: Option<ActiveCloudNotifications>,
    cloud_authority: Option<SessionAuthority>,
    cloud_validated: bool,
    cloud_routes: BTreeSet<PhysicalDeviceId>,
    last_observation: XiaomiAuthObservation,
    last_snapshot: AuthSnapshot,
    auth_recheck_revision: Option<crate::storage::AuthRevision>,
    next_auth: Instant,
    auth_backoff: Duration,
    next_network: Instant,
    next_catalog: Instant,
    epoch: u64,
}

type AuthTaskResult = (XiaomiAuthObservation, Result<AuthReport, AuthError>);
type NetworkTaskResult = (
    NetworkMonitor,
    Result<NetworkUpdate, crate::xiaomi::discovery::DiscoveryError>,
);

enum CatalogTaskResult {
    Owned {
        snapshot: AuthSnapshot,
        owned: Option<crate::xiaomi::cloud::OwnedCatalog>,
    },
    Resolved {
        snapshot: AuthSnapshot,
        result: Result<
            (
                crate::xiaomi::catalog::DeviceCatalog,
                HashMap<String, String>,
            ),
            CatalogRefreshError,
        >,
    },
}

enum CatalogRefreshError {
    Storage(StorageError),
    Remote,
}

type BrowserResult = (
    DiscoveryBrowser,
    Result<MdnsEvent, crate::xiaomi::discovery::DiscoveryError>,
);

struct ConnectingGateway {
    candidate: GatewayCandidate,
    network: NetworkUpdate,
    account: crate::device::AccountId,
    session_generation: crate::storage::AuthSessionGeneration,
    authority: SessionAuthority,
    control: GatewayConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, GatewayConnectionFactResult>,
    attempt: Option<u64>,
}

#[derive(Clone)]
struct GatewayReady {
    attempt: u64,
    endpoint: GatewayEndpoint,
    handle: crate::xiaomi::gateway::GatewayHandle,
    evidence: crate::xiaomi::gateway::GatewayEvidence,
    authority: SessionAuthority,
}

struct GatewayConnectionConfig {
    candidate: GatewayCandidate,
    endpoints: Vec<(GatewayEndpoint, crate::xiaomi::discovery::NetworkInterface)>,
    network: NetworkUpdate,
    virtual_did: String,
    private_key_pem: String,
    certificate_pem: String,
    authority: SessionAuthority,
    setup: LanSetupSlots,
    #[cfg(test)]
    startup: RefCell<
        Option<LocalBoxFuture<'static, Result<super::RunningGateway, TransportStartupError>>>,
    >,
}

struct GatewayAttemptScope(SessionAuthority);

impl Drop for GatewayAttemptScope {
    fn drop(&mut self) {
        self.0.revoke();
    }
}

struct ActiveGateway {
    did: u64,
    handle: crate::xiaomi::gateway::GatewayHandle,
    authority: SessionAuthority,
    root_authority: SessionAuthority,
    attempt: u64,
    wire_generation: u64,
    proof: AuthenticatedGateway,
    control: GatewayConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, GatewayConnectionFactResult>,
    publication: Option<LocalBoxFuture<'static, GatewayOperationResult>>,
    desired_dids: BTreeSet<String>,
    selected_dids: BTreeSet<String>,
    refresh_pending: bool,
    tokens: BTreeMap<PhysicalDeviceId, SubscriptionToken>,
}

#[derive(Clone)]
struct GatewayConnectionControl {
    desired: Rc<RefCell<BTreeSet<String>>>,
    refresh: Rc<Cell<u64>>,
    stopped: Rc<Cell<bool>>,
    publication: Rc<Cell<GatewayPublication>>,
    changed: Rc<Event>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GatewayPublication {
    Pending,
    Active,
    Rejected,
}

enum GatewayConnectionFact {
    Ready(Box<GatewayReady>),
    Disconnected {
        attempt: u64,
    },
    Notification(GatewayNotification),
    Operation(GatewayOperationResult),
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

type GatewayConnectionFactResult = (
    flume::Receiver<GatewayConnectionFact>,
    Result<GatewayConnectionFact, flume::RecvError>,
);

struct LanConnectionConfig {
    physical: PhysicalDeviceId,
    targets: Vec<LanTarget>,
    network: NetworkUpdate,
    account: crate::device::AccountId,
    session_generation: crate::storage::AuthSessionGeneration,
    descriptor: crate::xiaomi::catalog::FeatureDescriptor,
    property: LanProperty,
    virtual_did: u64,
    setup: LanSetupSlots,
    #[cfg(test)]
    startup: RefCell<
        Option<LocalBoxFuture<'static, Result<super::RunningLan, crate::xiaomi::lan::LanError>>>,
    >,
}

struct ActiveLan {
    targets: Vec<LanTarget>,
    control: LanConnectionControl,
    current: Option<(u64, Rc<LanAttemptResources>)>,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, LanConnectionFactResult>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LanPublication {
    Pending,
    Active,
    Rejected,
}

#[derive(Clone)]
struct LanConnectionControl {
    stopped: Rc<Cell<bool>>,
    reconnect: Rc<Cell<u64>>,
    desired_push: Rc<Cell<bool>>,
    push_active: Rc<Cell<bool>>,
    publication: Rc<Cell<LanPublication>>,
    changed: Rc<Event>,
    runtime_wake: Rc<Event>,
    #[cfg(test)]
    ready_seen: Rc<Cell<u64>>,
    #[cfg(test)]
    ready_stage: Rc<Cell<u8>>,
}

#[derive(Clone)]
struct LanReady {
    generation: u64,
    attempt: u64,
    target: LanTarget,
    network: NetworkUpdate,
    account: crate::device::AccountId,
    session_generation: crate::storage::AuthSessionGeneration,
    descriptor: crate::xiaomi::catalog::FeatureDescriptor,
    evidence: crate::xiaomi::lan::LanEvidence,
    operation: LanOperationEvidence,
    resources: Rc<LanAttemptResources>,
}

enum LanConnectionFact {
    Ready(Box<LanReady>),
    Disconnected {
        generation: u64,
        attempt: u64,
    },
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

type LanConnectionFactResult = (
    flume::Receiver<LanConnectionFact>,
    Result<LanConnectionFact, flume::RecvError>,
);

struct LanAttemptResources {
    device: PhysicalDeviceId,
    handle: crate::xiaomi::lan::LanHandle,
    authority: SessionAuthority,
    registry: CurrentSessionRegistry,
    state: StateRuntime,
    route: RefCell<Option<super::RouteLease>>,
    token: RefCell<Option<SubscriptionToken>>,
    closed: Cell<bool>,
}

struct LanAttemptScope(Rc<LanAttemptResources>);

#[derive(Clone)]
struct LanSetupSlots {
    active: Rc<Cell<usize>>,
    available: Rc<Event>,
    limit: usize,
}

struct LanSetupSlot(LanSetupSlots);

struct ActiveCloudNotifications {
    authority: SessionAuthority,
    control: CloudConnectionControl,
    task: LocalBoxFuture<'static, ()>,
    fact: LocalBoxFuture<'static, CloudConnectionFactResult>,
    desired: BTreeSet<String>,
    generation: u64,
    session_id: u64,
    tokens: BTreeMap<PhysicalDeviceId, SubscriptionToken>,
    publication: Option<LocalBoxFuture<'static, CloudSelectionResult>>,
}

#[derive(Clone)]
struct CloudConnectionControl {
    desired: Rc<RefCell<BTreeSet<String>>>,
    stopped: Rc<Cell<bool>>,
    changed: Rc<Event>,
}

enum CloudConnectionFact {
    Notification(CloudNotification),
    Selection(CloudSelectionResult),
    Disconnected,
    Failure {
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
    },
}

struct CloudConnectionConfig {
    oauth_client_uuid: String,
    access_token: String,
}

type CloudConnectionFactResult = (
    flume::Receiver<CloudConnectionFact>,
    Result<CloudConnectionFact, flume::RecvError>,
);

struct CloudSelectionResult {
    desired: BTreeSet<String>,
    result: Result<u64, crate::xiaomi::mqtt::MqttError>,
}

enum GatewayOperationResult {
    Selected {
        did: u64,
        desired: BTreeSet<String>,
        result: Result<u64, crate::xiaomi::gateway::GatewayError>,
    },
    Refreshed {
        did: u64,
        result:
            Result<crate::xiaomi::gateway::GatewayEvidence, crate::xiaomi::gateway::GatewayError>,
    },
}

impl GatewayConnectionControl {
    fn new() -> Self {
        Self {
            desired: Rc::new(RefCell::new(BTreeSet::new())),
            refresh: Rc::new(Cell::new(0)),
            stopped: Rc::new(Cell::new(false)),
            publication: Rc::new(Cell::new(GatewayPublication::Pending)),
            changed: Rc::new(Event::new()),
        }
    }

    fn set_desired(&self, desired: BTreeSet<String>) {
        if *self.desired.borrow() != desired {
            *self.desired.borrow_mut() = desired;
            self.changed.notify(usize::MAX);
        }
    }

    fn refresh(&self) {
        self.refresh.set(self.refresh.get().wrapping_add(1));
        self.changed.notify(usize::MAX);
    }

    fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }

    fn publish(&self, publication: GatewayPublication) {
        self.publication.set(publication);
        self.changed.notify(usize::MAX);
    }
}

impl CloudConnectionControl {
    fn new(desired: BTreeSet<String>) -> Self {
        Self {
            desired: Rc::new(RefCell::new(desired)),
            stopped: Rc::new(Cell::new(false)),
            changed: Rc::new(Event::new()),
        }
    }

    fn set_desired(&self, desired: BTreeSet<String>) {
        if *self.desired.borrow() != desired {
            *self.desired.borrow_mut() = desired;
            self.changed.notify(usize::MAX);
        }
    }

    fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }
}

impl LanConnectionControl {
    fn new(runtime_wake: Rc<Event>) -> Self {
        Self {
            stopped: Rc::new(Cell::new(false)),
            reconnect: Rc::new(Cell::new(1)),
            desired_push: Rc::new(Cell::new(false)),
            push_active: Rc::new(Cell::new(false)),
            publication: Rc::new(Cell::new(LanPublication::Pending)),
            changed: Rc::new(Event::new()),
            runtime_wake,
            #[cfg(test)]
            ready_seen: Rc::new(Cell::new(0)),
            #[cfg(test)]
            ready_stage: Rc::new(Cell::new(0)),
        }
    }

    fn stop(&self) {
        self.stopped.set(true);
        self.changed.notify(usize::MAX);
    }

    fn reconnect(&self) {
        self.publication.set(LanPublication::Pending);
        self.reconnect
            .set(self.reconnect.get().wrapping_add(1).max(1));
        self.changed.notify(usize::MAX);
    }

    fn activate(&self) {
        self.publication.set(LanPublication::Active);
        self.changed.notify(usize::MAX);
    }

    fn reject(&self) {
        self.publication.set(LanPublication::Rejected);
        self.changed.notify(usize::MAX);
    }

    fn set_desired_push(&self, selected: bool) {
        if self.desired_push.replace(selected) != selected {
            self.changed.notify(usize::MAX);
        }
    }

    fn set_push_active(&self, active: bool) {
        if self.push_active.replace(active) != active {
            self.runtime_wake.notify(usize::MAX);
        }
    }
}

impl LanAttemptResources {
    fn install_route(&self, descriptor: Option<crate::xiaomi::catalog::FeatureDescriptor>) {
        let lease = self.registry.install_lan(
            self.device.clone(),
            self.handle.clone(),
            self.authority.clone(),
            descriptor,
        );
        self.route.replace(Some(lease));
    }

    fn replace_token(&self, token: Option<SubscriptionToken>) {
        if let Some(previous) = self.token.replace(token) {
            self.state.source_failed(&previous);
        }
    }

    fn token(&self) -> Option<SubscriptionToken> {
        self.token.borrow().clone()
    }

    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.authority.revoke();
        self.handle.stop();
        if let Some(token) = self.token.borrow_mut().take() {
            self.state.source_failed(&token);
        }
        if let Some(lease) = self.route.borrow_mut().take() {
            self.registry.revoke_lan_if_authority(&self.device, &lease);
        }
    }
}

impl Drop for LanAttemptResources {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for LanAttemptScope {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl LanSetupSlots {
    fn new(limit: usize) -> Self {
        Self {
            active: Rc::new(Cell::new(0)),
            available: Rc::new(Event::new()),
            limit,
        }
    }

    async fn acquire(&self, control: &LanConnectionControl) -> Option<LanSetupSlot> {
        loop {
            let listener = self.available.listen();
            let changed = control.changed.listen();
            if control.stopped.get() {
                return None;
            }
            if self.active.get() < self.limit {
                self.active.set(self.active.get() + 1);
                return Some(LanSetupSlot(self.clone()));
            }
            future::or(listener, changed).await;
        }
    }

    async fn acquire_gateway(&self, control: &GatewayConnectionControl) -> Option<LanSetupSlot> {
        loop {
            let listener = self.available.listen();
            let changed = control.changed.listen();
            if control.stopped.get() {
                return None;
            }
            if self.active.get() < self.limit {
                self.active.set(self.active.get() + 1);
                return Some(LanSetupSlot(self.clone()));
            }
            future::or(listener, changed).await;
        }
    }
}

impl Drop for LanSetupSlot {
    fn drop(&mut self) {
        self.0.active.set(self.0.active.get().saturating_sub(1));
        self.0.available.notify(1);
    }
}

impl XiaomiRuntime {
    fn session_authority(&self) -> SessionAuthority {
        SessionAuthority::new()
    }

    fn gateway_session_authority(&self) -> SessionAuthority {
        let expires_at = self
            .inner
            .auth_report
            .borrow()
            .certificate
            .map_or(0, |validity| validity.not_after);
        SessionAuthority::new_until(expires_at)
    }

    pub fn new(store: Store) -> Result<Self, XiaomiRuntimeError> {
        let xiaomi = store.xiaomi();
        let observer = xiaomi.auth_observer();
        let snapshot = observer
            .snapshot()
            .map_err(|failure| observer_failure(&xiaomi, failure))?;
        let auth = AuthService::new(store.xiaomi(), CloudClient::new()?);
        let cloud = Rc::new(CloudClient::new()?);
        let auth_report = auth.local_status()?;
        let service = DeviceService::new();
        let registry = CurrentSessionRegistry::default();
        let refresh_requested = Rc::new(Cell::new(true));
        let wake = Rc::new(Event::new());
        {
            let refresh_requested = refresh_requested.clone();
            let wake = wake.clone();
            registry.set_auth_refresh(move || {
                refresh_requested.set(true);
                wake.notify(usize::MAX);
            });
        }
        {
            let wake = wake.clone();
            registry.set_route_wake(move || {
                wake.notify(usize::MAX);
            });
        }
        let transports = Rc::new(RuntimeTransports::new(registry.clone()));
        let commands = CommandRuntime::new(service.clone(), transports.clone());
        let runtime_devices = store.devices();
        let state = StateRuntime::new(
            service.clone(),
            runtime_devices.clone(),
            transports,
            commands.execution_gate(),
            commands.subscribe_completions(),
        );
        let admission = AdmissionController::new(
            runtime_devices.clone(),
            service.clone(),
            commands.clone(),
            NetworkEpoch::new(0),
            snapshot.session_generation,
        );
        admission.restore_archived()?;
        let initial_status = XiaomiRuntimeStatus {
            admission: admission.snapshot()?,
            candidates: Vec::new(),
            gateways: Vec::new(),
            diagnostics: Vec::new(),
        };
        let last_observation = observation(&snapshot);
        Ok(Self {
            inner: Rc::new(Inner {
                service,
                auth_report: RefCell::new(auth_report),
                status: RefCell::new(initial_status),
                diagnostics: RefCell::new(VecDeque::new()),
                failure: RefCell::new(None),
                failed: Event::new(),
                refresh_requested,
                wake,
                running: Cell::new(false),
                stopped: Cell::new(false),
                runner: RefCell::new(Some(Runner {
                    auth: Rc::new(auth),
                    auth_task: None,
                    observer,
                    admission,
                    network: Some(NetworkMonitor::new()),
                    network_task: None,
                    cloud,
                    catalog_task: None,
                    catalog: None,
                    network_update: None,
                    discovery: None,
                    browser_read: None,
                    next_browser_start: Instant::now(),
                    connecting_gateways: BTreeMap::new(),
                    gateway_setup: LanSetupSlots::new(4),
                    gateways: BTreeMap::new(),
                    gateway_routes: BTreeMap::new(),
                    lans: BTreeMap::new(),
                    lan_setup: LanSetupSlots::new(4),
                    cloud_notifications: None,
                    cloud_authority: None,
                    cloud_validated: false,
                    cloud_routes: BTreeSet::new(),
                    last_observation,
                    last_snapshot: snapshot,
                    auth_recheck_revision: None,
                    next_auth: Instant::now(),
                    auth_backoff: AUTH_MAINTENANCE_INTERVAL,
                    next_network: Instant::now(),
                    next_catalog: Instant::now(),
                    epoch: 0,
                })),
                commands,
                state,
                registry,
                xiaomi,
                devices: runtime_devices,
            }),
        })
    }

    pub fn service(&self) -> DeviceService {
        self.inner.service.clone()
    }

    pub fn auth_report(&self) -> AuthReport {
        self.inner.auth_report.borrow().clone()
    }

    fn replace_auth_report(&self, report: AuthReport) {
        let changed = {
            let previous = self.inner.auth_report.borrow();
            previous.authentication != report.authentication
                || previous.certificate != report.certificate
                || previous.certificate_update != report.certificate_update
        };
        if changed {
            crate::terminal::log_status(&report, unix_time());
        }
        *self.inner.auth_report.borrow_mut() = report;
    }

    pub fn status(&self) -> XiaomiRuntimeStatus {
        let mut status = self.inner.status.borrow().clone();
        status.diagnostics = self.inner.diagnostics.borrow().iter().cloned().collect();
        status
    }

    pub fn refresh(&self) {
        let devices = self
            .inner
            .status
            .borrow()
            .admission
            .features
            .iter()
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>();
        for device in devices {
            self.inner.state.request_refresh(&device);
        }
        self.inner.refresh_requested.set(true);
        self.inner.wake.notify(usize::MAX);
    }

    pub fn stop(&self) {
        self.inner.stopped.set(true);
        self.inner.registry.revoke_all();
        self.inner.commands.stop();
        self.inner.state.stop();
        self.inner.wake.notify(usize::MAX);
    }

    pub fn check_failure(&self) -> Result<(), StorageError> {
        if let Some(error) = self.inner.failure.borrow().clone() {
            return Err(error);
        }
        Ok(())
    }

    pub async fn run(&self) -> Result<(), XiaomiRuntimeError> {
        self.check_failure()?;
        if self.inner.running.replace(true) {
            return Err(XiaomiRuntimeError::Storage(StorageError::new(
                self.inner.runner_path(),
                "start Xiaomi runtime",
                "Xiaomi runtime is already running",
            )));
        }
        self.inner.stopped.set(false);
        let mut runner = self.inner.runner.borrow_mut().take().ok_or_else(|| {
            XiaomiRuntimeError::Storage(StorageError::new(
                self.inner.runner_path(),
                "start Xiaomi runtime",
                "Xiaomi runtime cannot be restarted",
            ))
        })?;
        let _run_guard = RuntimeRunGuard {
            inner: self.inner.clone(),
        };
        let result = future::or(self.run_coordinator(&mut runner), async {
            future::or(self.inner.commands.run(), self.inner.state.run()).await;
            Ok(())
        })
        .await;
        if let Err(XiaomiRuntimeError::Storage(error)) = &result {
            self.record_failure(error.clone());
        }
        result
    }

    async fn run_coordinator(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        loop {
            if self.inner.stopped.get() {
                return Ok(());
            }
            self.drain_transport_failures(runner)?;
            for diagnostic in self.inner.commands.drain_diagnostics() {
                self.record_diagnostic(XiaomiRuntimeDiagnostic::Command(diagnostic));
            }
            for diagnostic in self.inner.state.drain_diagnostics() {
                self.record_diagnostic(XiaomiRuntimeDiagnostic::State(diagnostic));
            }
            let now = Instant::now();
            let requested = self.inner.refresh_requested.get();
            if runner.browser_read.is_none()
                && now >= runner.next_browser_start
                && let Some(network) = runner.network_update.as_ref()
            {
                match DiscoveryBrowser::start(&network.snapshot) {
                    Ok(browser) => runner.browser_read = Some(browser_read(browser)),
                    Err(_) => {
                        runner.next_browser_start = now + Duration::from_secs(1);
                        self.record_diagnostic(XiaomiRuntimeDiagnostic::Boundary(
                            XiaomiBoundaryDiagnostic {
                                component: XiaomiRuntimeComponent::Network,
                                stage: XiaomiFailureStage::Discover,
                                code: XiaomiSafeFailureCode::Unavailable,
                                subject: None,
                            },
                        ));
                    }
                }
            }
            if runner.network_task.is_none()
                && (now >= runner.next_network || requested)
                && let Some(mut network) = runner.network.take()
            {
                runner.network_task = Some(
                    async move {
                        let result = network.refresh(ROUTE_TIMEOUT).await;
                        (network, result)
                    }
                    .boxed_local(),
                );
                runner.next_network = now + NETWORK_REFRESH_INTERVAL;
            }
            let credentials_available = match runner.observer.observe() {
                Ok(observed) => {
                    if observed.session_generation != runner.last_observation.session_generation
                        || observed.uid != runner.last_observation.uid
                    {
                        runner.auth_task = None;
                        let snapshot = runner
                            .observer
                            .snapshot()
                            .map_err(|failure| self.observer_error(&runner.observer, failure))?;
                        self.inner.registry.revoke_all();
                        runner.cloud_routes.clear();
                        if let Some(authority) = runner.cloud_authority.take() {
                            authority.revoke();
                        }
                        self.drop_all_gateways(runner)?;
                        self.drop_all_lans(runner)?;
                        self.drop_cloud_notifications(runner);
                        runner.catalog_task = None;
                        runner.catalog = None;
                        runner.cloud_validated = false;
                        runner.admission.observe_auth(&snapshot)?;
                        self.reconcile(&runner.admission)?;
                        runner.last_observation = observation(&snapshot);
                        runner.last_snapshot = snapshot;
                        runner.next_auth = Instant::now();
                        runner.next_catalog = Instant::now();
                    } else if observed.revision != runner.last_observation.revision {
                        if runner.auth_task.take().is_some() {
                            runner.auth_recheck_revision = Some(observed.revision);
                        }
                        match runner.observer.snapshot() {
                            Ok(snapshot) => self.apply_auth_revision(runner, snapshot)?,
                            Err(failure) => {
                                return Err(self.observer_error(&runner.observer, failure).into());
                            }
                        }
                    } else {
                        runner.last_observation = observed;
                    }
                    true
                }
                Err(failure) => {
                    let error = self.observer_error(&runner.observer, failure);
                    self.record_failure(error.clone());
                    return Err(error.into());
                }
            };
            let now = Instant::now();
            if credentials_available
                && runner.auth_task.is_none()
                && (now >= runner.next_auth || requested)
            {
                let auth = runner.auth.clone();
                let scope = runner.last_observation.clone();
                runner.auth_task = Some(
                    async move {
                        let result = auth.check().await;
                        (scope, result)
                    }
                    .boxed_local(),
                );
                runner.next_auth = now + AUTH_MAINTENANCE_INTERVAL;
            }
            if credentials_available
                && runner.catalog_task.is_none()
                && (now >= runner.next_catalog || requested)
            {
                match runner.observer.snapshot() {
                    Ok(snapshot) => {
                        runner.catalog_task = Some(catalog_task(runner.cloud.clone(), snapshot));
                        runner.next_catalog = now + CATALOG_REFRESH_INTERVAL;
                    }
                    Err(failure) => {
                        let error = self.observer_error(&runner.observer, failure);
                        self.record_failure(error.clone());
                        return Err(error.into());
                    }
                }
            }
            if requested
                && (runner.network.is_none() || runner.network_task.is_some())
                && (!credentials_available
                    || (runner.auth_task.is_some() && runner.catalog_task.is_some()))
            {
                self.inner.refresh_requested.set(false);
            }
            self.schedule_gateway(runner)?;
            self.schedule_lan(runner)?;
            self.schedule_cloud_notifications(runner)?;
            self.update_lan_push_policy(runner);

            let mut deadline = Instant::now() + AUTH_OBSERVE_INTERVAL;
            if runner.network_task.is_none() {
                deadline = deadline.min(runner.next_network);
            }
            if credentials_available {
                if runner.auth_task.is_none() {
                    deadline = deadline.min(runner.next_auth);
                }
                if runner.catalog_task.is_none() {
                    deadline = deadline.min(runner.next_catalog);
                }
            }
            if runner.browser_read.is_none() && runner.network_update.is_some() {
                deadline = deadline.min(runner.next_browser_start);
            }
            let listener = self.inner.wake.listen();
            if self.inner.stopped.get() {
                continue;
            }
            enum Idle {
                Wake,
                Mdns(BrowserResult),
                GatewayStopped(u64),
                GatewayFact(u64, GatewayConnectionFactResult),
                GatewayPublication(GatewayOperationResult),
                LanStopped(PhysicalDeviceId),
                LanFact(PhysicalDeviceId, LanConnectionFactResult),
                CloudStopped,
                CloudFact(CloudConnectionFactResult),
                CloudPublication(CloudSelectionResult),
                Auth(AuthTaskResult),
                Network(NetworkTaskResult),
                Catalog(CatalogTaskResult),
            }
            let mut waits = Vec::<LocalBoxFuture<'_, Idle>>::new();
            waits.push(
                future::or(listener, async {
                    Timer::at(deadline).await;
                })
                .map(|_| Idle::Wake)
                .boxed_local(),
            );
            if let Some(read) = runner.browser_read.as_mut() {
                waits.push(read.as_mut().map(Idle::Mdns).boxed_local());
            }
            if let Some(cloud) = runner.cloud_notifications.as_mut() {
                waits.push(
                    cloud
                        .task
                        .as_mut()
                        .map(|_| Idle::CloudStopped)
                        .boxed_local(),
                );
                if let Some(publication) = cloud.publication.as_mut() {
                    waits.push(
                        publication
                            .as_mut()
                            .map(Idle::CloudPublication)
                            .boxed_local(),
                    );
                }
                waits.push(cloud.fact.as_mut().map(Idle::CloudFact).boxed_local());
            }
            for (device, lan) in &mut runner.lans {
                let stopped = device.clone();
                waits.push(
                    lan.task
                        .as_mut()
                        .map(move |_| Idle::LanStopped(stopped))
                        .boxed_local(),
                );
                let fact_device = device.clone();
                waits.push(
                    lan.fact
                        .as_mut()
                        .map(move |event| Idle::LanFact(fact_device, event))
                        .boxed_local(),
                );
            }
            if let Some(task) = runner.auth_task.as_mut() {
                waits.push(task.as_mut().map(Idle::Auth).boxed_local());
            }
            if let Some(task) = runner.network_task.as_mut() {
                waits.push(task.as_mut().map(Idle::Network).boxed_local());
            }
            if let Some(task) = runner.catalog_task.as_mut() {
                waits.push(task.as_mut().map(Idle::Catalog).boxed_local());
            }
            for (&did, gateway) in &mut runner.gateways {
                waits.push(
                    gateway
                        .task
                        .as_mut()
                        .map(move |_| Idle::GatewayStopped(did))
                        .boxed_local(),
                );
                waits.push(
                    gateway
                        .fact
                        .as_mut()
                        .map(move |fact| Idle::GatewayFact(did, fact))
                        .boxed_local(),
                );
                if let Some(publication) = gateway.publication.as_mut() {
                    waits.push(
                        publication
                            .as_mut()
                            .map(Idle::GatewayPublication)
                            .boxed_local(),
                    );
                }
            }
            for (&did, gateway) in &mut runner.connecting_gateways {
                waits.push(
                    gateway
                        .task
                        .as_mut()
                        .map(move |_| Idle::GatewayStopped(did))
                        .boxed_local(),
                );
                waits.push(
                    gateway
                        .fact
                        .as_mut()
                        .map(move |fact| Idle::GatewayFact(did, fact))
                        .boxed_local(),
                );
            }
            let (idle, _, _) = select_all(waits).await;
            let handled = (|| -> Result<(), XiaomiRuntimeError> {
                match idle {
                    Idle::Wake => {}
                    Idle::Mdns((browser, event)) => match event {
                        Ok(event) => {
                            runner.browser_read = Some(browser_read(browser));
                            if let Some(discovery) = runner.discovery.as_mut()
                                && discovery.apply(event).is_ok()
                            {
                                self.reconcile_gateway_candidates(runner)?;
                                self.update_status(runner);
                            }
                        }
                        Err(_) => {
                            runner.browser_read = None;
                            runner.next_browser_start = Instant::now() + Duration::from_secs(1);
                            self.record_diagnostic(XiaomiRuntimeDiagnostic::Boundary(
                                XiaomiBoundaryDiagnostic {
                                    component: XiaomiRuntimeComponent::Network,
                                    stage: XiaomiFailureStage::Discover,
                                    code: XiaomiSafeFailureCode::Unavailable,
                                    subject: None,
                                },
                            ));
                        }
                    },
                    Idle::GatewayStopped(did) => {
                        if runner.gateways.contains_key(&did) {
                            self.drop_gateway(runner, did)?;
                        } else if let Some(connecting) = runner.connecting_gateways.remove(&did) {
                            connecting.control.stop();
                            connecting.authority.revoke();
                        }
                    }
                    Idle::GatewayFact(did, (receiver, fact)) => {
                        if let Some(gateway) = runner.gateways.get_mut(&did) {
                            gateway.fact = gateway_connection_fact(receiver);
                        } else if let Some(gateway) = runner.connecting_gateways.get_mut(&did) {
                            gateway.fact = gateway_connection_fact(receiver);
                        }
                        match fact {
                            Ok(GatewayConnectionFact::Ready(ready)) => {
                                self.finish_gateway_ready(runner, did, *ready)?
                            }
                            Ok(GatewayConnectionFact::Disconnected { attempt }) => {
                                self.finish_gateway_disconnected(runner, did, attempt)?
                            }
                            Ok(GatewayConnectionFact::Notification(notification)) => {
                                self.apply_gateway_notification(runner, did, notification)?
                            }
                            Ok(GatewayConnectionFact::Operation(result)) => {
                                self.finish_gateway_operation(runner, result)?
                            }
                            Ok(GatewayConnectionFact::Failure { stage, code }) => self
                                .record_boundary(
                                    XiaomiRuntimeComponent::Gateway,
                                    stage,
                                    code,
                                    Some(did.to_string()),
                                ),
                            Err(_) => self.drop_gateway(runner, did)?,
                        }
                    }
                    Idle::GatewayPublication(result) => {
                        self.finish_gateway_operation(runner, result)?
                    }
                    Idle::LanStopped(device) => self.drop_lan(runner, &device)?,
                    Idle::LanFact(device, (receiver, fact)) => {
                        if let Some(lan) = runner.lans.get_mut(&device) {
                            lan.fact = lan_connection_fact(receiver);
                        }
                        match fact {
                            Ok(LanConnectionFact::Ready(ready)) => {
                                self.finish_lan_ready(runner, &device, *ready)?
                            }
                            Ok(LanConnectionFact::Disconnected {
                                generation,
                                attempt,
                            }) => {
                                self.finish_lan_disconnected(runner, &device, generation, attempt)?
                            }
                            Ok(LanConnectionFact::Failure { stage, code }) => self.record_boundary(
                                XiaomiRuntimeComponent::Lan,
                                stage,
                                code,
                                Some(device.parent_did.as_str().to_owned()),
                            ),
                            Err(_) => self.drop_lan(runner, &device)?,
                        }
                    }
                    Idle::CloudStopped => self.drop_cloud_notifications(runner),
                    Idle::CloudFact((receiver, fact)) => {
                        if let Some(cloud) = runner.cloud_notifications.as_mut() {
                            cloud.fact = cloud_connection_fact(receiver);
                        }
                        match fact {
                            Ok(CloudConnectionFact::Notification(notification)) => {
                                self.apply_cloud_notification(runner, notification)?
                            }
                            Ok(CloudConnectionFact::Selection(selection)) => {
                                self.finish_cloud_selection(runner, selection)?
                            }
                            Ok(CloudConnectionFact::Disconnected) => {
                                self.finish_cloud_disconnected(runner)?
                            }
                            Ok(CloudConnectionFact::Failure { stage, code }) => self
                                .record_boundary(XiaomiRuntimeComponent::Cloud, stage, code, None),
                            Err(_) => self.drop_cloud_notifications(runner),
                        }
                    }
                    Idle::CloudPublication(selection) => {
                        self.finish_cloud_selection(runner, selection)?
                    }
                    Idle::Auth((scope, result)) => {
                        runner.auth_task = None;
                        if let Err(AuthError::Storage(error)) = &result {
                            self.record_failure(error.clone());
                            return Err(error.clone().into());
                        }
                        let current_scope = match runner.observer.observe() {
                            Ok(current) => current,
                            Err(failure) => {
                                return Err(self.observer_error(&runner.observer, failure).into());
                            }
                        };
                        if current_scope.session_generation != scope.session_generation
                            || current_scope.uid != scope.uid
                            || current_scope.revision != scope.revision
                        {
                            if result.as_ref().is_ok_and(|report| report.is_success())
                                && current_scope.session_generation == scope.session_generation
                                && current_scope.uid == scope.uid
                            {
                                runner.auth_recheck_revision = Some(current_scope.revision);
                            }
                            runner.next_auth = Instant::now();
                            return Ok(());
                        }
                        match result {
                            Ok(report) => {
                                runner.auth_recheck_revision = None;
                                let successful = report.is_success();
                                if matches!(
                                    report.authentication,
                                    crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                                ) {
                                    if let Some(catalog) = runner.catalog.as_ref() {
                                        runner.admission.observe_cloud(&CloudEvidence {
                                            account: catalog.account.clone(),
                                            session_generation: catalog.session_generation,
                                            status: CloudStatus::InvalidToken,
                                        })?;
                                        let snapshot = runner.admission.snapshot()?;
                                        for feature in &snapshot.features {
                                            self.inner
                                                .registry
                                                .revoke_cloud(&feature.identity.physical);
                                        }
                                        self.inner.state.reconcile(&snapshot);
                                        self.inner.status.borrow_mut().admission = snapshot;
                                    }
                                    runner.cloud_routes.clear();
                                    if let Some(authority) = runner.cloud_authority.take() {
                                        authority.revoke();
                                    }
                                    runner.cloud_validated = false;
                                }
                                let certificate_current =
                                    report.certificate.is_some_and(|validity| {
                                        validity.currently_valid(report.completed_at())
                                    });
                                if !certificate_current {
                                    self.drop_all_gateways(runner)?;
                                }
                                self.replace_auth_report(report);
                                if successful && let Ok(snapshot) = runner.observer.snapshot() {
                                    runner.next_auth = auth_maintenance_deadline(&snapshot);
                                    runner.auth_backoff = AUTH_MAINTENANCE_INTERVAL;
                                } else if !successful {
                                    runner.next_auth = Instant::now() + runner.auth_backoff;
                                    runner.auth_backoff =
                                        (runner.auth_backoff * 2).min(Duration::from_secs(5 * 60));
                                }
                            }
                            Err(AuthError::Storage(error)) => {
                                self.record_failure(error.clone());
                                return Err(error.into());
                            }
                            Err(_) => {
                                runner.next_auth = Instant::now() + runner.auth_backoff;
                                runner.auth_backoff =
                                    (runner.auth_backoff * 2).min(Duration::from_secs(5 * 60));
                            }
                        }
                    }
                    Idle::Network((network, result)) => {
                        runner.network_task = None;
                        runner.network = Some(network);
                        self.finish_network_refresh(runner, result)?;
                        runner.next_network = Instant::now() + NETWORK_REFRESH_INTERVAL;
                    }
                    Idle::Catalog(result) => {
                        runner.catalog_task = None;
                        self.finish_catalog_refresh(runner, result)?;
                    }
                }
                Ok(())
            })();
            handled?;
        }
    }

    fn finish_network_refresh(
        &self,
        runner: &mut Runner,
        result: Result<NetworkUpdate, crate::xiaomi::discovery::DiscoveryError>,
    ) -> Result<(), XiaomiRuntimeError> {
        match result {
            Ok(update) if runner.network_update.as_ref() == Some(&update) => return Ok(()),
            Ok(update) => {
                self.revoke_network(runner)?;
                runner.epoch = update.epoch.get();
                runner.admission.invalidate(update.epoch);
                self.sync_cloud_routes(runner)?;
                self.reconcile(&runner.admission)?;
                runner.discovery = Some(DiscoveryRegistry::new(
                    update.epoch,
                    update.snapshot.clone(),
                ));
                match DiscoveryBrowser::start(&update.snapshot) {
                    Ok(browser) => runner.browser_read = Some(browser_read(browser)),
                    Err(_) => {
                        runner.browser_read = None;
                        runner.next_browser_start = Instant::now() + Duration::from_secs(1);
                        self.record_boundary(
                            XiaomiRuntimeComponent::Network,
                            XiaomiFailureStage::Discover,
                            XiaomiSafeFailureCode::Unavailable,
                            None,
                        );
                    }
                }
                runner.network_update = Some(update);
            }
            Err(_) if runner.network_update.is_some() => {
                self.record_boundary(
                    XiaomiRuntimeComponent::Network,
                    XiaomiFailureStage::Discover,
                    XiaomiSafeFailureCode::Unavailable,
                    None,
                );
                self.revoke_network(runner)?;
                runner.epoch = runner.epoch.saturating_add(1);
                runner.admission.invalidate(NetworkEpoch::new(runner.epoch));
                runner.discovery = None;
                runner.browser_read = None;
                runner.network_update = None;
                self.reconcile(&runner.admission)?;
            }
            Err(_) => self.record_boundary(
                XiaomiRuntimeComponent::Network,
                XiaomiFailureStage::Discover,
                XiaomiSafeFailureCode::Unavailable,
                None,
            ),
        }
        Ok(())
    }

    fn drain_transport_failures(&self, _runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        for failure in self.inner.registry.drain_route_failures() {
            match failure {
                super::RouteFailure::Lan { device, lease } => {
                    if self.inner.registry.revoke_lan_if_authority(&device, &lease) {
                        self.reconnect_lan(_runner, &device)?;
                    }
                }
                super::RouteFailure::CloudUnauthorized { device, lease } => {
                    if !self
                        .inner
                        .registry
                        .revoke_cloud_if_authority(&device, &lease)
                    {
                        continue;
                    }
                    _runner.cloud_routes.remove(&device);
                    if let Some(catalog) = _runner.catalog.as_ref() {
                        _runner.admission.observe_cloud(&CloudEvidence {
                            account: catalog.account.clone(),
                            session_generation: _runner.last_snapshot.session_generation,
                            status: CloudStatus::InvalidToken,
                        })?;
                        self.reconcile(&_runner.admission)?;
                    }
                    self.inner.refresh_requested.set(true);
                }
            }
        }
        Ok(())
    }

    fn reconcile_gateway_candidates(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let candidates = runner
            .discovery
            .as_ref()
            .map(DiscoveryRegistry::candidates)
            .unwrap_or_default();
        let stale_connecting = runner
            .connecting_gateways
            .iter()
            .filter(|(did, connecting)| {
                !candidates.iter().any(|candidate| {
                    candidate.gateway_did == **did
                        && candidate.home_group == connecting.candidate.home_group
                })
            })
            .map(|(&did, _)| did)
            .collect::<Vec<_>>();
        for did in stale_connecting {
            if let Some(connecting) = runner.connecting_gateways.remove(&did) {
                connecting.control.stop();
                connecting.authority.revoke();
            }
        }
        let stale = runner
            .gateways
            .iter()
            .filter(|(_, gateway)| {
                !candidates.iter().any(|candidate| {
                    candidate.gateway_did == gateway.did
                        && candidate.home_group == gateway.proof.candidate.home_group
                        && candidate
                            .endpoints
                            .contains(&gateway.proof.selected_endpoint)
                })
            })
            .map(|(&did, _)| did)
            .collect::<Vec<_>>();
        for did in stale {
            self.drop_gateway(runner, did)?;
        }
        Ok(())
    }

    fn apply_auth_revision(
        &self,
        runner: &mut Runner,
        snapshot: AuthSnapshot,
    ) -> Result<(), XiaomiRuntimeError> {
        let previous = runner.last_snapshot.record.as_ref();
        let current = snapshot.record.as_ref();
        let token_changed = previous.map(|record| &record.tokens.access_token)
            != current.map(|record| &record.tokens.access_token);
        let gateway_identity_changed = previous.map(|record| {
            (
                &record.virtual_did,
                &record.private_key_pem,
                &record.certificate_pem,
            )
        }) != current.map(|record| {
            (
                &record.virtual_did,
                &record.private_key_pem,
                &record.certificate_pem,
            )
        });
        let previous_snapshot = std::mem::replace(&mut runner.last_snapshot, snapshot);
        let applied = (|| -> Result<(), XiaomiRuntimeError> {
            if token_changed
                && let Some(account) = runner.catalog.as_ref().map(|c| c.account.clone())
            {
                self.drop_cloud_notifications(runner);
                runner.cloud_validated = true;
                runner.admission.observe_cloud(&CloudEvidence {
                    account,
                    session_generation: runner.last_snapshot.session_generation,
                    status: CloudStatus::Ready,
                })?;
                self.sync_cloud_routes(runner)?;
                self.reconcile(&runner.admission)?;
            }
            if gateway_identity_changed {
                self.drop_all_gateways(runner)?;
            }
            Ok(())
        })();
        match applied {
            Ok(()) => {
                runner.last_observation = observation(&runner.last_snapshot);
                runner.catalog_task = None;
                runner.next_auth =
                    if runner.auth_recheck_revision == Some(runner.last_snapshot.revision) {
                        Instant::now()
                    } else {
                        auth_maintenance_deadline(&runner.last_snapshot)
                    };
                Ok(())
            }
            Err(error) => {
                runner.last_snapshot = previous_snapshot;
                Err(error)
            }
        }
    }

    fn revoke_network(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        self.drop_all_gateways(runner)?;
        self.drop_all_lans(runner)?;
        self.drop_cloud_notifications(runner);
        Ok(())
    }

    fn finish_catalog_refresh(
        &self,
        runner: &mut Runner,
        task: CatalogTaskResult,
    ) -> Result<(), XiaomiRuntimeError> {
        let task_snapshot = match &task {
            CatalogTaskResult::Owned { snapshot, .. }
            | CatalogTaskResult::Resolved { snapshot, .. } => snapshot,
        };
        let current = runner
            .observer
            .observe()
            .map_err(|failure| self.observer_error(&runner.observer, failure))?;
        if current.revision != task_snapshot.revision
            || current.session_generation != task_snapshot.session_generation
        {
            return Ok(());
        }
        match task {
            CatalogTaskResult::Owned { snapshot, owned } => {
                let Some(owned) = owned else {
                    self.record_boundary(
                        XiaomiRuntimeComponent::Catalog,
                        XiaomiFailureStage::Read,
                        XiaomiSafeFailureCode::Unavailable,
                        None,
                    );
                    runner.next_catalog = Instant::now() + AUTH_MAINTENANCE_INTERVAL;
                    return Ok(());
                };
                let Some(record) = snapshot.record.as_ref() else {
                    return Ok(());
                };
                runner.cloud_validated = true;
                let specifications = runner
                    .catalog
                    .as_ref()
                    .map(|catalog| catalog.specifications.clone())
                    .unwrap_or_default();
                let mut catalog = match assemble_catalog(&owned, &HashMap::new()) {
                    Ok(catalog) => catalog,
                    Err(_) => {
                        self.record_boundary(
                            XiaomiRuntimeComponent::Catalog,
                            XiaomiFailureStage::Read,
                            XiaomiSafeFailureCode::Protocol,
                            None,
                        );
                        runner.next_catalog = Instant::now() + AUTH_MAINTENANCE_INTERVAL;
                        return Ok(());
                    }
                };
                if let Some(previous) = runner.catalog.as_ref() {
                    for device in &mut catalog.devices {
                        if let Some(old) = previous.catalog.devices.iter().find(|old| {
                            old.home_id == device.home_id
                                && old.parent_did == device.parent_did
                                && old.model == device.model
                                && old.spec_type == device.spec_type
                        }) {
                            device.features = old.features.clone();
                        }
                    }
                }
                let admission_catalog = super::AdmissionCatalog {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(
                                self.inner.runner_path(),
                                "validate Xiaomi account",
                                error,
                            )
                        },
                    )?,
                    session_generation: snapshot.session_generation,
                    catalog,
                    specifications,
                };
                runner
                    .admission
                    .apply_complete_catalog(&admission_catalog)?;
                let admission_snapshot = runner.admission.snapshot()?;
                self.rebuild_gateway_routes(runner, &admission_snapshot);
                self.schedule_gateway_operations(runner);
                let session_generation = admission_catalog.session_generation;
                runner.catalog = Some(admission_catalog);
                runner.admission.observe_cloud(&CloudEvidence {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(
                                self.inner.runner_path(),
                                "validate Xiaomi account",
                                error,
                            )
                        },
                    )?,
                    session_generation,
                    status: CloudStatus::Ready,
                })?;
                self.sync_cloud_routes(runner)?;
                self.reconcile(&runner.admission)?;
                self.update_status(runner);
                runner.catalog_task = Some(catalog_specs_task(
                    runner.cloud.clone(),
                    self.inner.devices(),
                    snapshot,
                    owned,
                ));
            }
            CatalogTaskResult::Resolved { snapshot, result } => {
                let (catalog, specifications) = match result {
                    Err(CatalogRefreshError::Storage(error)) => return Err(error.into()),
                    Err(CatalogRefreshError::Remote) => {
                        self.record_boundary(
                            XiaomiRuntimeComponent::Catalog,
                            XiaomiFailureStage::Read,
                            XiaomiSafeFailureCode::Unavailable,
                            None,
                        );
                        runner.next_catalog = Instant::now() + AUTH_MAINTENANCE_INTERVAL;
                        return Ok(());
                    }
                    Ok(result) => result,
                };
                let Some(record) = snapshot.record.as_ref() else {
                    return Ok(());
                };
                let admission_catalog = super::AdmissionCatalog {
                    account: crate::device::AccountId::new(record.uid.clone()).map_err(
                        |error| {
                            StorageError::new(
                                self.inner.runner_path(),
                                "validate Xiaomi account",
                                error,
                            )
                        },
                    )?,
                    session_generation: snapshot.session_generation,
                    catalog,
                    specifications,
                };
                runner
                    .admission
                    .apply_complete_catalog(&admission_catalog)?;
                let admission_snapshot = runner.admission.snapshot()?;
                self.rebuild_gateway_routes(runner, &admission_snapshot);
                self.sync_cloud_routes(runner)?;
                self.inner.state.reconcile(&admission_snapshot);
                self.inner.status.borrow_mut().admission = admission_snapshot;
                self.schedule_gateway_operations(runner);
                let persisted = persist_catalog(
                    &self.inner.devices(),
                    &admission_catalog.catalog,
                    &admission_catalog.specifications,
                    unix_time(),
                    snapshot.revision,
                )?;
                if persisted {
                    runner.catalog = Some(admission_catalog);
                    runner.next_catalog = Instant::now() + CATALOG_REFRESH_INTERVAL;
                    self.update_status(runner);
                }
            }
        }
        Ok(())
    }

    fn schedule_lan(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let (Some(catalog), Some(network), Some(record)) = (
            runner.catalog.as_ref(),
            runner.network_update.as_ref(),
            runner.last_snapshot.record.as_ref(),
        ) else {
            return Ok(());
        };
        let account = catalog.account.clone();
        let devices = catalog.catalog.devices.clone();
        let network = network.clone();
        let virtual_did = record.virtual_did.clone();
        let binding_snapshot = runner.admission.snapshot()?;
        let Some(binding) = binding_snapshot.binding else {
            return Ok(());
        };
        let stale = runner
            .lans
            .iter()
            .filter_map(|(physical, lan)| {
                let current = devices.iter().find(|device| {
                    device.home_id == binding.home.as_str()
                        && device.parent_did == physical.parent_did.as_str()
                });
                (!current.is_some_and(|device| {
                    lan.targets
                        .iter()
                        .any(|target| target.matches_catalog(device, binding.home.as_str()))
                }))
                .then_some(physical.clone())
            })
            .collect::<Vec<_>>();
        for physical in stale {
            self.drop_lan(runner, &physical)?;
        }
        let virtual_did = virtual_did.parse::<u64>().ok().filter(|did| *did != 0);
        let Some(virtual_did) = virtual_did else {
            return Ok(());
        };
        for device in &devices {
            let Ok(parent_did) = crate::device::DeviceDid::new(device.parent_did.clone()) else {
                continue;
            };
            let physical = crate::device::PhysicalDeviceId {
                account: account.clone(),
                home: binding.home.clone(),
                parent_did,
            };
            if device.home_id != binding.home.as_str() || runner.lans.contains_key(&physical) {
                continue;
            }
            let Some(descriptor) = device
                .features
                .iter()
                .find(|feature| feature.properties.iter().any(|property| property.readable))
                .cloned()
            else {
                continue;
            };
            let Some(property) = descriptor
                .properties
                .iter()
                .find(|property| property.readable)
                .map(|property| LanProperty {
                    siid: property.siid,
                    piid: property.piid,
                })
            else {
                continue;
            };
            let targets = network
                .snapshot
                .interfaces()
                .iter()
                .filter_map(|interface| {
                    LanTarget::from_catalog(
                        device,
                        binding.home.as_str(),
                        interface.clone(),
                        network.epoch,
                    )
                    .ok()
                })
                .collect::<Vec<_>>();
            if targets.is_empty() {
                continue;
            }
            let control = LanConnectionControl::new(self.inner.wake.clone());
            let (fact_sender, fact_receiver) = flume::bounded(2);
            let config = LanConnectionConfig {
                physical: physical.clone(),
                targets: targets.clone(),
                network: network.clone(),
                account: account.clone(),
                session_generation: runner.last_snapshot.session_generation,
                descriptor,
                property,
                virtual_did,
                setup: runner.lan_setup.clone(),
                #[cfg(test)]
                startup: RefCell::new(None),
            };
            let task = lan_connection_lifetime(
                config,
                control.clone(),
                fact_sender,
                self.inner.registry.clone(),
                self.inner.state.clone(),
            );
            runner.lans.insert(
                physical,
                ActiveLan {
                    targets,
                    control,
                    current: None,
                    task,
                    fact: lan_connection_fact(fact_receiver),
                },
            );
        }
        Ok(())
    }

    fn finish_lan_ready(
        &self,
        runner: &mut Runner,
        device: &PhysicalDeviceId,
        ready: LanReady,
    ) -> Result<(), XiaomiRuntimeError> {
        #[cfg(test)]
        if let Some(active) = runner.lans.get(device) {
            active
                .control
                .ready_seen
                .set(active.control.ready_seen.get().saturating_add(1));
        }
        let Some(active) = runner.lans.get(device) else {
            ready.resources.close();
            return Ok(());
        };
        if active.control.reconnect.get() != ready.generation || active.control.stopped.get() {
            ready.resources.close();
            return Ok(());
        }
        if ready.resources.closed.get() {
            return Ok(());
        }
        if let Some((attempt, resources)) = active.current.as_ref() {
            if Rc::ptr_eq(resources, &ready.resources) {
                active.control.activate();
                return Ok(());
            }
            if *attempt >= ready.attempt {
                ready.resources.close();
                return Ok(());
            }
        }
        let legacy_operation = match ready.operation {
            LanOperationEvidence::Mcn02Legacy => LegacyOperationEvidence::SuccessfulRead,
            _ => LegacyOperationEvidence::Unverified,
        };
        let proof = AuthenticatedLan {
            account: ready.account,
            session_generation: ready.session_generation,
            target: ready.target,
            network: ready.network.snapshot,
            evidence: ready.evidence.clone(),
            legacy_operation,
        };
        let Some(catalog) = runner.catalog.as_ref() else {
            ready.resources.close();
            active.control.reject();
            return Ok(());
        };
        let confirmed = runner.admission.confirm_lan(&proof, catalog)?;
        if !confirmed {
            #[cfg(test)]
            active.control.ready_stage.set(3);
            ready.resources.close();
            active.control.reject();
            return Ok(());
        }
        let snapshot = runner.admission.snapshot()?;
        ready.resources.install_route(
            (!proof.evidence.native_supported
                && proof.legacy_operation == LegacyOperationEvidence::SuccessfulRead)
                .then_some(ready.descriptor),
        );
        self.sync_cloud_routes(runner)?;
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot;
        let active = runner
            .lans
            .get_mut(device)
            .expect("LAN connection remained registered while publishing");
        if let Some((_, previous)) = active.current.replace((ready.attempt, ready.resources)) {
            previous.close();
        }
        active.control.activate();
        #[cfg(test)]
        active.control.ready_stage.set(4);
        self.update_lan_push_policy(runner);
        Ok(())
    }

    fn finish_lan_disconnected(
        &self,
        runner: &mut Runner,
        device: &PhysicalDeviceId,
        generation: u64,
        attempt: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(active) = runner.lans.get_mut(device) else {
            return Ok(());
        };
        if generation != active.control.reconnect.get()
            || active
                .current
                .as_ref()
                .is_some_and(|(current, _)| *current != attempt)
        {
            return Ok(());
        }
        if let Some((_, resources)) = active.current.take() {
            resources.close();
        }
        active.control.set_push_active(false);
        runner.admission.remove_lan(device);
        let snapshot = runner.admission.snapshot()?;
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot;
        self.select_fallback_push_sources(runner)?;
        Ok(())
    }

    fn update_lan_push_policy(&self, runner: &mut Runner) {
        for (device, lan) in &runner.lans {
            let gateway_selected = runner
                .gateways
                .values()
                .any(|gateway| gateway.tokens.contains_key(device));
            lan.control.set_desired_push(!gateway_selected);
        }
    }

    fn schedule_cloud_notifications(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let snapshot = runner.admission.snapshot()?;
        let devices = if snapshot.status == super::AdmissionStatus::Active {
            snapshot
                .features
                .iter()
                .map(|feature| feature.identity.physical.clone())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let desired = devices
            .iter()
            .filter(|device| {
                !runner
                    .gateways
                    .values()
                    .any(|gateway| gateway.tokens.contains_key(*device))
                    && !runner
                        .lans
                        .get(*device)
                        .is_some_and(|lan| lan.control.push_active.get())
            })
            .map(|device| device.parent_did.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if let Some(cloud) = runner.cloud_notifications.as_mut() {
            cloud.control.set_desired(desired);
            return Ok(());
        }
        if desired.is_empty() {
            return Ok(());
        }
        let (Some(network), Some(record)) = (
            runner.network_update.as_ref(),
            runner.last_snapshot.record.as_ref(),
        ) else {
            return Ok(());
        };
        let interfaces = network.snapshot.interfaces().to_vec();
        if interfaces.is_empty() {
            return Ok(());
        }
        let authority = self.session_authority();
        let config = CloudConnectionConfig {
            oauth_client_uuid: record.oauth_client_uuid.clone(),
            access_token: record.tokens.access_token.clone(),
        };
        let control = CloudConnectionControl::new(desired);
        let (fact_sender, fact_receiver) = flume::bounded(256);
        runner.cloud_notifications = Some(ActiveCloudNotifications {
            authority: authority.clone(),
            task: cloud_connection_lifetime(config, authority, control.clone(), fact_sender),
            fact: cloud_connection_fact(fact_receiver),
            control,
            desired: BTreeSet::new(),
            generation: 0,
            session_id: 0,
            tokens: BTreeMap::new(),
            publication: None,
        });
        Ok(())
    }

    fn finish_cloud_disconnected(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = runner.cloud_notifications.as_mut() else {
            return Ok(());
        };
        for (_, token) in std::mem::take(&mut cloud.tokens) {
            self.inner.state.source_failed(&token);
        }
        cloud.publication = None;
        cloud.desired.clear();
        cloud.generation = 0;
        cloud.session_id = 0;
        self.select_fallback_push_sources(runner)
    }

    fn finish_cloud_selection(
        &self,
        runner: &mut Runner,
        completed: CloudSelectionResult,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = runner.cloud_notifications.as_mut() else {
            return Ok(());
        };
        cloud.publication = None;
        let CloudSelectionResult { desired, result } = completed;
        let generation = match result {
            Ok(generation) => generation,
            Err(error) if error.kind() == &crate::xiaomi::mqtt::MqttErrorKind::Superseded => {
                return Ok(());
            }
            Err(error) => {
                self.record_boundary(
                    XiaomiRuntimeComponent::Cloud,
                    XiaomiFailureStage::Subscribe,
                    mqtt_failure_code(error.kind()),
                    None,
                );
                for (_, token) in std::mem::take(&mut cloud.tokens) {
                    self.inner.state.source_failed(&token);
                }
                cloud.desired.clear();
                cloud.generation = 0;
                cloud.session_id = 0;
                return Ok(());
            }
        };
        for (_, token) in std::mem::take(&mut cloud.tokens) {
            self.inner.state.source_failed(&token);
        }
        cloud.desired = desired.clone();
        cloud.generation = generation;
        cloud.session_id = generation;
        if *cloud.control.desired.borrow() != cloud.desired {
            return Ok(());
        }
        let snapshot = runner.admission.snapshot()?;
        for physical in snapshot
            .features
            .iter()
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>()
        {
            if cloud.desired.contains(physical.parent_did.as_str())
                && let Some(token) = self.inner.state.select_push_source(
                    &physical,
                    PushSource::Cloud,
                    cloud.session_id,
                    generation,
                )
            {
                self.inner
                    .state
                    .acknowledge(&token, cloud.session_id, generation);
                cloud.tokens.insert(physical, token);
            }
        }
        Ok(())
    }

    fn select_fallback_push_sources(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let devices = runner
            .admission
            .snapshot()?
            .features
            .into_iter()
            .map(|feature| feature.identity.physical)
            .collect::<BTreeSet<_>>();
        for device in devices {
            let gateway_selected = runner
                .gateways
                .values()
                .any(|gateway| gateway.tokens.contains_key(&device));
            if let Some(lan) = runner.lans.get(&device) {
                lan.control.set_desired_push(!gateway_selected);
            }
            let lan_selected = !gateway_selected
                && runner
                    .lans
                    .get(&device)
                    .is_some_and(|lan| lan.control.push_active.get());
            if gateway_selected || lan_selected {
                if let Some(cloud) = runner.cloud_notifications.as_mut()
                    && let Some(token) = cloud.tokens.remove(&device)
                {
                    self.inner.state.source_failed(&token);
                }
                continue;
            }
            if let Some(cloud) = runner.cloud_notifications.as_mut()
                && cloud.desired.contains(device.parent_did.as_str())
                && !cloud.tokens.contains_key(&device)
                && let Some(token) = self.inner.state.select_push_source(
                    &device,
                    PushSource::Cloud,
                    cloud.session_id,
                    cloud.generation,
                )
            {
                self.inner
                    .state
                    .acknowledge(&token, cloud.session_id, cloud.generation);
                cloud.tokens.insert(device, token);
            }
        }
        Ok(())
    }

    fn apply_cloud_notification(
        &self,
        runner: &mut Runner,
        notification: CloudNotification,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(cloud) = runner.cloud_notifications.as_ref() else {
            return Ok(());
        };
        let did = match &notification {
            CloudNotification::Property { did, .. }
            | CloudNotification::Event { did, .. }
            | CloudNotification::State { did, .. } => did,
        };
        let Some((device, token)) = cloud
            .tokens
            .iter()
            .find(|(device, _)| device.parent_did.as_str() == did)
        else {
            return Ok(());
        };
        match notification {
            CloudNotification::Property {
                siid,
                piid,
                value,
                generation,
                ..
            } => {
                self.inner.state.apply_property(
                    token,
                    cloud.session_id,
                    generation,
                    siid,
                    piid,
                    value.as_ref(),
                    unix_time(),
                    false,
                );
            }
            CloudNotification::Event {
                siid,
                eiid,
                arguments,
                generation,
                ..
            } => match arguments {
                EventArguments::Keyed(arguments) => {
                    self.inner.state.apply_keyed_event(
                        token,
                        cloud.session_id,
                        generation,
                        siid,
                        eiid,
                        &arguments,
                        unix_time(),
                        false,
                    );
                }
                EventArguments::Positional(arguments) => {
                    self.inner.state.apply_positional_event(
                        token,
                        cloud.session_id,
                        generation,
                        siid,
                        eiid,
                        &arguments,
                        unix_time(),
                        false,
                    );
                }
            },
            CloudNotification::State {
                online, generation, ..
            } => {
                if self
                    .inner
                    .state
                    .apply_cloud_online(token, cloud.session_id, generation, online)
                    .is_some()
                    && runner.admission.observe_cloud_online(device, online)
                {
                    let snapshot = runner.admission.snapshot()?;
                    self.sync_cloud_routes(runner)?;
                    self.inner.state.reconcile(&snapshot);
                    self.inner.status.borrow_mut().admission = snapshot;
                }
            }
        }
        Ok(())
    }

    fn drop_lan(
        &self,
        runner: &mut Runner,
        device: &PhysicalDeviceId,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(lan) = runner.lans.remove(device) else {
            return Ok(());
        };
        lan.control.stop();
        if let Some((_, resources)) = lan.current {
            resources.close();
        }
        runner.admission.remove_lan(device);
        let snapshot = runner.admission.snapshot()?;
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot;
        self.select_fallback_push_sources(runner)?;
        Ok(())
    }

    fn reconnect_lan(
        &self,
        runner: &mut Runner,
        device: &PhysicalDeviceId,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(lan) = runner.lans.get_mut(device) else {
            return Ok(());
        };
        if let Some((_, resources)) = lan.current.take() {
            resources.close();
        }
        lan.control.set_push_active(false);
        lan.control.reconnect();
        runner.admission.remove_lan(device);
        let snapshot = runner.admission.snapshot()?;
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot;
        self.select_fallback_push_sources(runner)?;
        Ok(())
    }

    fn drop_all_lans(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let devices = runner.lans.keys().cloned().collect::<Vec<_>>();
        for device in devices {
            self.drop_lan(runner, &device)?;
        }
        Ok(())
    }

    fn drop_cloud_notifications(&self, runner: &mut Runner) {
        if let Some(cloud) = runner.cloud_notifications.take() {
            cloud.control.stop();
            cloud.authority.revoke();
            for (_, token) in cloud.tokens {
                self.inner.state.source_failed(&token);
            }
        }
    }

    fn schedule_gateway(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let (Some(catalog), Some(network), Some(discovery)) = (
            runner.catalog.as_ref(),
            runner.network_update.as_ref(),
            runner.discovery.as_ref(),
        ) else {
            return Ok(());
        };
        let Some(candidate) = discovery.candidates().into_iter().find(|candidate| {
            catalog
                .catalog
                .homes
                .iter()
                .any(|home| home.group_id == candidate.home_group)
                && !runner.gateways.contains_key(&candidate.gateway_did)
                && !runner
                    .connecting_gateways
                    .contains_key(&candidate.gateway_did)
        }) else {
            return Ok(());
        };
        let endpoints = candidate
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                network
                    .snapshot
                    .interfaces_with_index(endpoint.interface_index)
                    .find(|interface| interface.address() == endpoint.source_address)
                    .cloned()
                    .map(|interface| (endpoint.clone(), interface))
            })
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Ok(());
        }
        let snapshot = runner
            .observer
            .snapshot()
            .map_err(|failure| self.observer_error(&runner.observer, failure))?;
        let Some(record) = snapshot.record else {
            return Ok(());
        };
        if record.uid != catalog.account.as_str() {
            return Ok(());
        }
        let authority = self.gateway_session_authority();
        let config = GatewayConnectionConfig {
            candidate: candidate.clone(),
            endpoints,
            network: network.clone(),
            virtual_did: record.virtual_did,
            private_key_pem: record.private_key_pem,
            certificate_pem: record.certificate_pem,
            authority: authority.clone(),
            setup: runner.gateway_setup.clone(),
            #[cfg(test)]
            startup: RefCell::new(None),
        };
        let control = GatewayConnectionControl::new();
        let (fact_sender, fact_receiver) = flume::bounded(256);
        runner.connecting_gateways.insert(
            candidate.gateway_did,
            ConnectingGateway {
                candidate: candidate.clone(),
                network: network.clone(),
                account: catalog.account.clone(),
                session_generation: snapshot.session_generation,
                authority,
                task: gateway_full_connection_lifetime(config, control.clone(), fact_sender),
                fact: gateway_connection_fact(fact_receiver),
                control,
                attempt: None,
            },
        );
        Ok(())
    }

    fn finish_gateway_ready(
        &self,
        runner: &mut Runner,
        did: u64,
        ready: GatewayReady,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(connecting) = runner.connecting_gateways.get_mut(&did) else {
            return Ok(());
        };
        if !ready.authority.check() {
            return Ok(());
        }
        connecting.attempt = Some(ready.attempt);
        let Some(catalog) = runner.catalog.as_ref() else {
            connecting.control.publish(GatewayPublication::Rejected);
            return Ok(());
        };
        let proof = AuthenticatedGateway {
            account: connecting.account.clone(),
            session_generation: connecting.session_generation,
            candidate: connecting.candidate.clone(),
            selected_endpoint: ready.endpoint.clone(),
            network: connecting.network.snapshot.clone(),
            evidence: ready.evidence.clone(),
        };
        if let Err(error) = runner.admission.observe_gateway(proof.clone(), catalog) {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Err(error.into());
        }
        if runner.cloud_validated
            && let Err(error) = runner.admission.observe_cloud(&CloudEvidence {
                account: catalog.account.clone(),
                session_generation: connecting.session_generation,
                status: CloudStatus::Ready,
            })
        {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Err(error.into());
        }
        let snapshot = match runner.admission.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                connecting.control.publish(GatewayPublication::Rejected);
                connecting.authority.revoke();
                return Err(error.into());
            }
        };
        let auth = runner
            .observer
            .snapshot()
            .map_err(|failure| self.observer_error(&runner.observer, failure))?;
        if auth.session_generation != connecting.session_generation || auth.record.is_none() {
            connecting.control.publish(GatewayPublication::Rejected);
            connecting.authority.revoke();
            return Ok(());
        }
        self.sync_cloud_routes(runner)?;
        let connecting = runner
            .connecting_gateways
            .remove(&did)
            .expect("connecting gateway remained present");
        connecting.control.publish(GatewayPublication::Active);
        runner.gateways.insert(
            did,
            ActiveGateway {
                did,
                handle: ready.handle,
                authority: ready.authority,
                root_authority: connecting.authority,
                attempt: ready.attempt,
                wire_generation: 0,
                proof,
                control: connecting.control,
                task: connecting.task,
                fact: connecting.fact,
                publication: None,
                desired_dids: BTreeSet::new(),
                selected_dids: BTreeSet::new(),
                refresh_pending: false,
                tokens: BTreeMap::new(),
            },
        );
        if snapshot.status == super::AdmissionStatus::SuspendedConflict {
            self.suspend_gateway_routes(runner, &snapshot);
        } else {
            self.rebuild_gateway_routes(runner, &snapshot);
        }
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot.clone();
        self.schedule_gateway_operations(runner);
        self.update_status(runner);
        Ok(())
    }

    fn finish_gateway_disconnected(
        &self,
        runner: &mut Runner,
        did: u64,
        attempt: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        if let Some(connecting) = runner.connecting_gateways.get_mut(&did) {
            if connecting.attempt != Some(attempt) {
                return Ok(());
            }
            return self.cleanup_disconnected_gateway(runner, did);
        }
        let network = runner
            .network_update
            .clone()
            .expect("an active gateway has a current network snapshot");
        let Some(gateway) = runner.gateways.remove(&did) else {
            return Ok(());
        };
        if gateway.attempt != attempt {
            runner.gateways.insert(did, gateway);
            return Ok(());
        }
        for (device, token) in gateway.tokens {
            self.inner.state.source_failed(&token);
            self.inner.registry.revoke_gateway(&device);
        }
        let stale = runner
            .gateway_routes
            .iter()
            .filter_map(|(device, selected)| (*selected == did).then_some(device.clone()))
            .collect::<Vec<_>>();
        for device in stale {
            runner.gateway_routes.remove(&device);
            self.inner.registry.revoke_gateway(&device);
        }
        gateway.control.publish(GatewayPublication::Pending);
        runner.connecting_gateways.insert(
            did,
            ConnectingGateway {
                candidate: gateway.proof.candidate.clone(),
                network,
                account: gateway.proof.account.clone(),
                session_generation: gateway.proof.session_generation,
                authority: gateway.root_authority,
                control: gateway.control,
                task: gateway.task,
                fact: gateway.fact,
                attempt: Some(attempt),
            },
        );
        self.cleanup_disconnected_gateway(runner, did)
    }

    fn cleanup_disconnected_gateway(
        &self,
        runner: &mut Runner,
        did: u64,
    ) -> Result<(), XiaomiRuntimeError> {
        runner.admission.remove_gateway(did)?;
        let snapshot = runner.admission.snapshot()?;
        self.rebuild_gateway_routes(runner, &snapshot);
        self.inner.state.reconcile(&snapshot);
        self.schedule_gateway_operations(runner);
        self.update_status(runner);
        if let Some(connecting) = runner.connecting_gateways.get_mut(&did) {
            connecting.attempt = None;
        }
        Ok(())
    }

    fn sync_cloud_routes(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        let Some(record) = runner.last_snapshot.record.as_ref() else {
            return Ok(());
        };
        if runner.cloud_authority.is_none() {
            runner.cloud_authority = Some(self.session_authority());
        }
        let authority = runner
            .cloud_authority
            .as_ref()
            .expect("cloud authority was initialized")
            .clone();
        let snapshot = runner.admission.snapshot()?;
        let enabled = snapshot
            .features
            .iter()
            .filter(|feature| feature.paths.cloud)
            .map(|feature| feature.identity.physical.clone())
            .collect::<BTreeSet<_>>();
        for device in runner.cloud_routes.difference(&enabled) {
            self.inner.registry.revoke_cloud(device);
        }
        for feature in &snapshot.features {
            if enabled.contains(&feature.identity.physical) {
                self.inner.registry.install_cloud_if_changed(
                    feature.identity.physical.clone(),
                    runner.cloud.clone(),
                    record.tokens.access_token.clone(),
                    record.tokens.expires_at,
                    authority.clone(),
                );
            } else {
                self.inner.registry.revoke_cloud(&feature.identity.physical);
            }
        }
        runner.cloud_routes = enabled;
        Ok(())
    }

    fn rebuild_gateway_routes(&self, runner: &mut Runner, snapshot: &super::AdmissionSnapshot) {
        if snapshot.status == super::AdmissionStatus::SuspendedConflict {
            self.suspend_gateway_routes(runner, snapshot);
            return;
        }
        let mut routes = BTreeMap::new();
        let mut push = BTreeMap::new();
        for feature in &snapshot.features {
            if let Some(path) = feature
                .gateways
                .iter()
                .filter(|path| path.access && runner.gateways.contains_key(&path.gateway_did))
                .min_by_key(|path| path.gateway_did)
            {
                routes.insert(feature.identity.physical.clone(), path.gateway_did);
            }
            if let Some(path) = feature
                .gateways
                .iter()
                .filter(|path| path.push && runner.gateways.contains_key(&path.gateway_did))
                .min_by_key(|path| path.gateway_did)
            {
                push.insert(feature.identity.physical.clone(), path.gateway_did);
            }
        }
        let old_routes = std::mem::take(&mut runner.gateway_routes);
        for (device, old_did) in &old_routes {
            if routes.get(device) != Some(old_did) {
                self.inner.registry.revoke_gateway(device);
            }
        }
        for (device, did) in &routes {
            if old_routes.get(device) == Some(did) {
                continue;
            }
            let gateway = runner.gateways.get(did).expect("selected gateway is live");
            self.inner.registry.install_gateway(
                device.clone(),
                gateway.handle.clone(),
                gateway.authority.clone(),
            );
        }
        runner.gateway_routes = routes;
        for gateway in runner.gateways.values_mut() {
            let did = gateway.did;
            let desired = push
                .iter()
                .filter(|(_, selected)| **selected == did)
                .map(|(device, _)| device.parent_did.as_str().to_owned())
                .collect();
            gateway.desired_dids = desired;
            gateway.control.set_desired(gateway.desired_dids.clone());
            let stale = gateway
                .tokens
                .keys()
                .filter(|device| push.get(*device) != Some(&did))
                .cloned()
                .collect::<Vec<_>>();
            for device in stale {
                if let Some(token) = gateway.tokens.remove(&device) {
                    self.inner.state.source_failed(&token);
                }
            }
        }
    }

    fn suspend_gateway_routes(&self, runner: &mut Runner, snapshot: &super::AdmissionSnapshot) {
        for feature in &snapshot.features {
            self.inner
                .registry
                .revoke_gateway(&feature.identity.physical);
        }
        runner.gateway_routes.clear();
        for gateway in runner.gateways.values_mut() {
            for (_, token) in std::mem::take(&mut gateway.tokens) {
                self.inner.state.source_failed(&token);
            }
            gateway.desired_dids.clear();
            gateway.control.set_desired(BTreeSet::new());
        }
    }

    fn schedule_gateway_operations(&self, runner: &mut Runner) {
        for gateway in runner.gateways.values_mut() {
            gateway.control.set_desired(gateway.desired_dids.clone());
            if gateway.refresh_pending {
                gateway.refresh_pending = false;
                gateway.control.refresh();
            }
        }
    }

    fn finish_gateway_operation(
        &self,
        runner: &mut Runner,
        result: GatewayOperationResult,
    ) -> Result<(), XiaomiRuntimeError> {
        match result {
            GatewayOperationResult::Selected {
                did,
                desired,
                result,
            } => {
                let Some(gateway) = runner.gateways.get_mut(&did) else {
                    return Ok(());
                };
                gateway.publication = None;
                let generation = match result {
                    Ok(generation) => generation,
                    Err(error)
                        if error.kind()
                            == &crate::xiaomi::gateway::GatewayErrorKind::Superseded =>
                    {
                        return Ok(());
                    }
                    Err(error) => {
                        self.record_boundary(
                            XiaomiRuntimeComponent::Gateway,
                            XiaomiFailureStage::Subscribe,
                            gateway_failure_code(error.kind()),
                            Some(did.to_string()),
                        );
                        gateway.selected_dids.clear();
                        for (_, token) in std::mem::take(&mut gateway.tokens) {
                            self.inner.state.source_failed(&token);
                        }
                        self.select_fallback_push_sources(runner)?;
                        return Ok(());
                    }
                };
                let snapshot = if gateway.desired_dids == desired {
                    Some(runner.admission.snapshot()?)
                } else {
                    None
                };
                let generation_changed = gateway.wire_generation != generation;
                gateway.selected_dids = desired.clone();
                gateway.wire_generation = generation;
                if let Some(snapshot) = snapshot {
                    if generation_changed {
                        for (_, token) in std::mem::take(&mut gateway.tokens) {
                            self.inner.state.source_failed(&token);
                        }
                    }
                    let physical = snapshot
                        .features
                        .iter()
                        .filter(|feature| {
                            desired.contains(feature.identity.physical.parent_did.as_str())
                        })
                        .map(|feature| feature.identity.physical.clone())
                        .collect::<BTreeSet<_>>();
                    for device in physical {
                        if gateway.tokens.contains_key(&device) {
                            continue;
                        }
                        if let Some(token) = self.inner.state.select_push_source(
                            &device,
                            PushSource::Gateway(did),
                            did,
                            generation,
                        ) {
                            self.inner.state.acknowledge(&token, did, generation);
                            gateway.tokens.insert(device, token);
                        }
                    }
                }
            }
            GatewayOperationResult::Refreshed { did, result } => {
                let Some(gateway) = runner.gateways.get_mut(&did) else {
                    return Ok(());
                };
                gateway.publication = None;
                let evidence = match result {
                    Ok(evidence) => evidence,
                    Err(error) => {
                        self.record_boundary(
                            XiaomiRuntimeComponent::Gateway,
                            XiaomiFailureStage::Read,
                            gateway_failure_code(error.kind()),
                            Some(did.to_string()),
                        );
                        self.drop_gateway(runner, did)?;
                        return Ok(());
                    }
                };
                gateway.proof.evidence = evidence;
                let Some(catalog) = runner.catalog.as_ref() else {
                    return Ok(());
                };
                if let Err(error) = runner
                    .admission
                    .observe_gateway(gateway.proof.clone(), catalog)
                {
                    self.discard_gateway_after_admission_failure(runner, did);
                    return Err(error.into());
                }
                let snapshot = runner.admission.snapshot()?;
                self.rebuild_gateway_routes(runner, &snapshot);
                self.sync_cloud_routes(runner)?;
                self.inner.state.reconcile(&snapshot);
                self.inner.status.borrow_mut().admission = snapshot.clone();
            }
        }
        self.schedule_gateway_operations(runner);
        self.update_status(runner);
        Ok(())
    }

    fn apply_gateway_notification(
        &self,
        runner: &mut Runner,
        gateway_did: u64,
        notification: GatewayNotification,
    ) -> Result<(), XiaomiRuntimeError> {
        let Some(gateway) = runner.gateways.get(&gateway_did) else {
            return Ok(());
        };
        match notification {
            GatewayNotification::DeviceListChanged { .. } => {
                if let Some(gateway) = runner.gateways.get_mut(&gateway_did) {
                    gateway.refresh_pending = true;
                }
                self.schedule_gateway_operations(runner);
                self.refresh();
            }
            GatewayNotification::Property {
                did,
                siid,
                piid,
                value,
                generation,
                ..
            } => {
                if let Some((_, token)) = gateway
                    .tokens
                    .iter()
                    .find(|(device, _)| device.parent_did.as_str() == did)
                {
                    self.inner.state.apply_property(
                        token,
                        gateway.did,
                        generation,
                        siid,
                        piid,
                        value.as_ref(),
                        unix_time(),
                        false,
                    );
                }
            }
            GatewayNotification::Event {
                did,
                siid,
                eiid,
                arguments,
                generation,
                ..
            } => {
                if let Some((_, token)) = gateway
                    .tokens
                    .iter()
                    .find(|(device, _)| device.parent_did.as_str() == did)
                {
                    match arguments {
                        EventArguments::Keyed(arguments) => {
                            self.inner.state.apply_keyed_event(
                                token,
                                gateway.did,
                                generation,
                                siid,
                                eiid,
                                &arguments,
                                unix_time(),
                                false,
                            );
                        }
                        EventArguments::Positional(arguments) => {
                            self.inner.state.apply_positional_event(
                                token,
                                gateway.did,
                                generation,
                                siid,
                                eiid,
                                &arguments,
                                unix_time(),
                                false,
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn drop_gateway(&self, runner: &mut Runner, did: u64) -> Result<(), XiaomiRuntimeError> {
        let Some(gateway) = runner.gateways.remove(&did) else {
            return Ok(());
        };
        gateway.control.stop();
        gateway.authority.revoke();
        gateway.root_authority.revoke();
        for (device, token) in gateway.tokens {
            self.inner.state.source_failed(&token);
            self.inner.registry.revoke_gateway(&device);
        }
        runner.admission.remove_gateway(gateway.did)?;
        let snapshot = runner.admission.snapshot()?;
        self.rebuild_gateway_routes(runner, &snapshot);
        self.inner.state.reconcile(&snapshot);
        self.schedule_gateway_operations(runner);
        self.update_status(runner);
        Ok(())
    }

    fn discard_gateway_after_admission_failure(&self, runner: &mut Runner, did: u64) {
        let Some(gateway) = runner.gateways.remove(&did) else {
            return;
        };
        gateway.control.stop();
        gateway.authority.revoke();
        gateway.root_authority.revoke();
        for (device, token) in gateway.tokens {
            self.inner.state.source_failed(&token);
            self.inner.registry.revoke_gateway(&device);
        }
        let stale = runner
            .gateway_routes
            .iter()
            .filter_map(|(device, selected)| (*selected == did).then_some(device.clone()))
            .collect::<Vec<_>>();
        for device in stale {
            runner.gateway_routes.remove(&device);
            self.inner.registry.revoke_gateway(&device);
        }
        let _ = runner.admission.remove_gateway(did);
    }

    fn drop_all_gateways(&self, runner: &mut Runner) -> Result<(), XiaomiRuntimeError> {
        for (_, connecting) in std::mem::take(&mut runner.connecting_gateways) {
            connecting.control.stop();
            connecting.authority.revoke();
        }
        let dids = runner.gateways.keys().copied().collect::<Vec<_>>();
        for did in dids {
            self.drop_gateway(runner, did)?;
        }
        Ok(())
    }

    fn reconcile(&self, admission: &AdmissionController) -> Result<(), XiaomiRuntimeError> {
        let snapshot = admission.snapshot()?;
        self.inner.state.reconcile(&snapshot);
        self.inner.status.borrow_mut().admission = snapshot;
        Ok(())
    }

    fn update_status(&self, runner: &Runner) {
        let admission = self.inner.status.borrow().admission.clone();
        let candidates = runner
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .catalog
                    .devices
                    .iter()
                    .map(|device| {
                        let admitted = admission.features.iter().any(|feature| {
                            feature.identity.physical.account == catalog.account
                                && feature.identity.physical.home.as_str() == device.home_id
                                && feature.identity.physical.parent_did.as_str()
                                    == device.parent_did
                        });
                        let state = if admission.status == super::AdmissionStatus::SuspendedConflict
                            && admitted
                        {
                            XiaomiCandidateState::Conflict
                        } else if device.features.is_empty() {
                            if device.spec_type.as_ref().is_some_and(|type_urn| {
                                catalog.specifications.contains_key(type_urn)
                                    && !crate::xiaomi::catalog::supports_spec(
                                        &device.model,
                                        type_urn,
                                    )
                            }) {
                                XiaomiCandidateState::Unsupported
                            } else {
                                XiaomiCandidateState::Unrecognized
                            }
                        } else if admitted && admission.status == super::AdmissionStatus::Active {
                            XiaomiCandidateState::Ready
                        } else {
                            XiaomiCandidateState::WaitingForGateway
                        };
                        XiaomiCandidateStatus {
                            account_id: catalog.account.as_str().to_owned(),
                            home_id: device.home_id.clone(),
                            did: device.parent_did.clone(),
                            parent_did: device.parent_did.clone(),
                            name: device.name.clone(),
                            model: device.model.clone(),
                            state,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let gateways = runner
            .discovery
            .as_ref()
            .map(|discovery| {
                discovery
                    .candidates()
                    .into_iter()
                    .map(|candidate| XiaomiGatewayStatus {
                        did: candidate.gateway_did,
                        home_group: candidate.home_group,
                        unverified: candidate.unverified,
                        authenticated: runner.gateways.contains_key(&candidate.gateway_did),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut status = self.inner.status.borrow_mut();
        status.candidates = candidates;
        status.gateways = gateways;
    }

    fn observer_error(
        &self,
        _observer: &XiaomiAuthObserver,
        failure: SessionCheckFailure,
    ) -> StorageError {
        observer_failure(&self.inner.xiaomi, failure)
    }

    fn record_failure(&self, error: StorageError) {
        self.inner.failure.borrow_mut().get_or_insert(error);
        self.inner.failed.notify(usize::MAX);
    }

    fn record_diagnostic(&self, diagnostic: XiaomiRuntimeDiagnostic) {
        let mut diagnostics = self.inner.diagnostics.borrow_mut();
        if diagnostics
            .back()
            .is_some_and(|current| same_diagnostic(current, &diagnostic))
        {
            return;
        }
        if diagnostics.len() == 32 {
            diagnostics.pop_front();
        }
        log::warn!("{}", format_diagnostic(&diagnostic));
        diagnostics.push_back(diagnostic);
    }

    fn record_boundary(
        &self,
        component: XiaomiRuntimeComponent,
        stage: XiaomiFailureStage,
        code: XiaomiSafeFailureCode,
        subject: Option<String>,
    ) {
        self.record_diagnostic(XiaomiRuntimeDiagnostic::Boundary(
            XiaomiBoundaryDiagnostic {
                component,
                stage,
                code,
                subject,
            },
        ));
    }
}

impl Inner {
    fn runner_path(&self) -> &std::path::Path {
        self.xiaomi.path()
    }

    fn devices(&self) -> crate::storage::DeviceStore {
        self.devices.clone()
    }
}

fn observation(snapshot: &AuthSnapshot) -> XiaomiAuthObservation {
    XiaomiAuthObservation {
        uid: snapshot.record.as_ref().map(|record| record.uid.clone()),
        revision: snapshot.revision,
        session_generation: snapshot.session_generation,
    }
}

fn same_diagnostic(left: &XiaomiRuntimeDiagnostic, right: &XiaomiRuntimeDiagnostic) -> bool {
    match (left, right) {
        (XiaomiRuntimeDiagnostic::Command(left), XiaomiRuntimeDiagnostic::Command(right)) => {
            left == right
        }
        (XiaomiRuntimeDiagnostic::State(left), XiaomiRuntimeDiagnostic::State(right)) => {
            left == right
        }
        (XiaomiRuntimeDiagnostic::Boundary(left), XiaomiRuntimeDiagnostic::Boundary(right)) => {
            left == right
        }
        _ => false,
    }
}

fn format_diagnostic(diagnostic: &XiaomiRuntimeDiagnostic) -> String {
    match diagnostic {
        XiaomiRuntimeDiagnostic::Command(super::RuntimeDiagnostic::CommandFailure {
            feature,
            command,
            path,
            stage,
            outcome,
            sent,
        }) => format!(
            "Xiaomi command failed: device={} service={} role={} command={command:?} path={path:?} stage={stage:?} outcome={outcome:?} sent={sent}",
            feature.physical.parent_did,
            feature.service_instance,
            feature.role.as_str(),
        ),
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::ReadFailure {
            feature,
            path,
            failure,
        }) => format!(
            "Xiaomi state read failed: device={} service={} role={} path={path:?} outcome={failure:?}",
            feature.physical.parent_did,
            feature.service_instance,
            feature.role.as_str(),
        ),
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::QueueFull) => {
            "Xiaomi state refresh queue is full".to_owned()
        }
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::StorageFailure(error)) => {
            format!("Xiaomi state persistence failed: {error}")
        }
        XiaomiRuntimeDiagnostic::Boundary(diagnostic) => format!(
            "Xiaomi boundary failure: component={:?} stage={:?} code={:?} subject={}",
            diagnostic.component,
            diagnostic.stage,
            diagnostic.code,
            diagnostic.subject.as_deref().unwrap_or("none"),
        ),
    }
}

fn observer_failure(
    store: &crate::storage::XiaomiStore,
    failure: SessionCheckFailure,
) -> StorageError {
    match failure {
        SessionCheckFailure::Storage(error) => error.into_storage_error(),
        SessionCheckFailure::InvalidCredentials => StorageError::new(
            store.path(),
            "validate Xiaomi authentication observation",
            "Stored Xiaomi credentials are invalid",
        ),
    }
}

fn catalog_task(
    cloud: Rc<CloudClient>,
    snapshot: AuthSnapshot,
) -> LocalBoxFuture<'static, CatalogTaskResult> {
    async move {
        let owned = match snapshot.record.as_ref() {
            Some(record) => cloud
                .get_owned_catalog_for_uid(&record.tokens.access_token, &record.uid)
                .await
                .ok(),
            None => None,
        };
        CatalogTaskResult::Owned { snapshot, owned }
    }
    .boxed_local()
}

fn catalog_specs_task(
    cloud: Rc<CloudClient>,
    devices: crate::storage::DeviceStore,
    snapshot: AuthSnapshot,
    owned: crate::xiaomi::cloud::OwnedCatalog,
) -> LocalBoxFuture<'static, CatalogTaskResult> {
    async move {
        let result = async {
            let types = owned
                .devices
                .iter()
                .filter_map(|device| device.spec_type.clone())
                .collect::<BTreeSet<_>>();
            let mut specifications = HashMap::new();
            for type_urn in types {
                let (document, cached) = match devices
                    .load_spec(&type_urn)
                    .map_err(CatalogRefreshError::Storage)?
                {
                    Some(cached) => (cached.document, true),
                    None => match cloud.get_spec_instance(&type_urn).await {
                        Ok(document) => (document, false),
                        Err(_) => continue,
                    },
                };
                let invalid = owned
                    .devices
                    .iter()
                    .filter(|device| device.spec_type.as_deref() == Some(type_urn.as_str()))
                    .find_map(|device| {
                        crate::xiaomi::catalog::compile_spec(&device.model, &document).err()
                    });
                if let Some(error) = invalid {
                    if cached {
                        return Err(CatalogRefreshError::Storage(StorageError::new(
                            devices.path(),
                            "compile cached Xiaomi device specification",
                            error,
                        )));
                    }
                    continue;
                }
                specifications.insert(type_urn, document);
            }
            // Specifications that failed independently remain absent. The catalog keeps
            // their ownership records with no controllable features.
            let catalog = assemble_catalog(&owned, &specifications)
                .map_err(|_| CatalogRefreshError::Remote)?;
            Ok((catalog, specifications))
        }
        .await;
        CatalogTaskResult::Resolved { snapshot, result }
    }
    .boxed_local()
}

fn browser_read(browser: DiscoveryBrowser) -> LocalBoxFuture<'static, BrowserResult> {
    async move {
        let result = browser.next_event().await;
        (browser, result)
    }
    .boxed_local()
}

fn lan_connection_fact(
    receiver: flume::Receiver<LanConnectionFact>,
) -> LocalBoxFuture<'static, LanConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

fn lan_connection_lifetime(
    config: LanConnectionConfig,
    control: LanConnectionControl,
    facts: flume::Sender<LanConnectionFact>,
    registry: CurrentSessionRegistry,
    state: StateRuntime,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut target_index = 0_usize;
        let mut next_attempt = 1_u64;
        let mut retry_delay = LOCAL_RETRY_INTERVAL;
        while !control.stopped.get() {
            let target = config.targets[target_index % config.targets.len()].clone();
            target_index = target_index.wrapping_add(1);
            let authority = SessionAuthority::new();
            let Some(setup_slot) = config.setup.acquire(&control).await else {
                return;
            };
            #[cfg(test)]
            let injected = config.startup.borrow_mut().take();
            #[cfg(not(test))]
            let injected: Option<
                LocalBoxFuture<'static, Result<super::RunningLan, crate::xiaomi::lan::LanError>>,
            > = None;
            let running = match injected {
                Some(startup) => startup.await,
                None => {
                    start_lan(
                        target.clone(),
                        config.virtual_did,
                        config.property,
                        Instant::now() + LOCAL_SESSION_TIMEOUT,
                        authority.lan_guard(),
                    )
                    .await
                }
            };
            drop(setup_slot);
            let running = match running {
                Ok(running) => running,
                Err(error) => {
                    let _ = facts.try_send(LanConnectionFact::Failure {
                        stage: XiaomiFailureStage::Authenticate,
                        code: lan_failure_code(error.kind()),
                    });
                    control.runtime_wake.notify(usize::MAX);
                    authority.revoke();
                    if !target_index.is_multiple_of(config.targets.len()) {
                        continue;
                    }
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
                    if !wait_for_lan_retry(&control, delay).await {
                        return;
                    }
                    continue;
                }
            };
            retry_delay = LOCAL_RETRY_INTERVAL;
            let operation = running.operation;
            let evidence = running.evidence.clone();
            let (handle, notifications, _, _, mut session) = running.into_parts();
            let resources = Rc::new(LanAttemptResources {
                device: config.physical.clone(),
                handle,
                authority,
                registry: registry.clone(),
                state: state.clone(),
                route: RefCell::new(None),
                token: RefCell::new(None),
                closed: Cell::new(false),
            });
            let _attempt_scope = LanAttemptScope(resources.clone());
            let generation = control.reconnect.get();
            let attempt = next_attempt;
            next_attempt = next_attempt.wrapping_add(1).max(1);
            let ready = LanReady {
                generation,
                attempt,
                target,
                network: config.network.clone(),
                account: config.account.clone(),
                session_generation: config.session_generation,
                descriptor: config.descriptor.clone(),
                evidence,
                operation,
                resources: resources.clone(),
            };
            let mut connected = true;
            while control.publication.get() == LanPublication::Pending
                && !control.stopped.get()
                && generation == control.reconnect.get()
            {
                if facts.is_empty() {
                    match facts.try_send(LanConnectionFact::Ready(Box::new(ready.clone()))) {
                        Ok(()) | Err(flume::TrySendError::Full(_)) => {}
                        Err(flume::TrySendError::Disconnected(_)) => return,
                    }
                }
                control.runtime_wake.notify(usize::MAX);
                enum PublicationWait {
                    Changed,
                    Retry,
                    Stopped,
                }
                let listener = control.changed.listen();
                let event = future::or(
                    async {
                        let _ = session.as_mut().await;
                        PublicationWait::Stopped
                    },
                    future::or(
                        async {
                            listener.await;
                            PublicationWait::Changed
                        },
                        async {
                            Timer::after(AUTH_OBSERVE_INTERVAL).await;
                            PublicationWait::Retry
                        },
                    ),
                )
                .await;
                if matches!(event, PublicationWait::Stopped) {
                    connected = false;
                    break;
                }
            }
            if connected
                && control.publication.get() == LanPublication::Active
                && generation == control.reconnect.get()
                && !control.stopped.get()
            {
                connected = run_active_lan_connection(
                    &config.physical,
                    generation,
                    &control,
                    &resources,
                    notifications,
                    &mut session,
                    &facts,
                )
                .await;
            }
            resources.close();
            control.set_push_active(false);
            let rejected = control.publication.get() == LanPublication::Rejected;
            control.publication.set(LanPublication::Pending);
            if facts
                .send_async(LanConnectionFact::Disconnected {
                    generation,
                    attempt,
                })
                .await
                .is_err()
            {
                return;
            }
            control.runtime_wake.notify(usize::MAX);
            if control.stopped.get() || rejected {
                return;
            }
            if connected && generation == control.reconnect.get() {
                continue;
            }
            let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
            if !wait_for_lan_retry(&control, delay).await {
                return;
            }
        }
    }
    .boxed_local()
}

async fn wait_for_lan_retry(control: &LanConnectionControl, delay: Duration) -> bool {
    let listener = control.changed.listen();
    future::or(
        async {
            Timer::after(delay).await;
        },
        async {
            listener.await;
        },
    )
    .await;
    !control.stopped.get()
}

async fn run_active_lan_connection(
    device: &PhysicalDeviceId,
    generation: u64,
    control: &LanConnectionControl,
    resources: &Rc<LanAttemptResources>,
    notifications: flume::Receiver<LanNotification>,
    session: &mut LocalBoxFuture<'static, Result<(), crate::xiaomi::lan::LanError>>,
    facts: &flume::Sender<LanConnectionFact>,
) -> bool {
    enum LanWireOperation {
        Subscribe(
            LocalBoxFuture<
                'static,
                Result<crate::xiaomi::lan::LanSubscription, crate::xiaomi::lan::LanError>,
            >,
        ),
        Unsubscribe(LocalBoxFuture<'static, Result<(), crate::xiaomi::lan::LanError>>),
    }
    let mut subscription = None::<crate::xiaomi::lan::LanSubscription>;
    let mut operation = None::<LanWireOperation>;
    let mut next_operation = Instant::now();
    let mut observed_desired = control.desired_push.get();
    loop {
        let listener = control.changed.listen();
        if control.stopped.get() || generation != control.reconnect.get() {
            return false;
        }
        let desired = control.desired_push.get();
        if desired != observed_desired {
            observed_desired = desired;
            next_operation = Instant::now();
        }
        if operation.is_none() && Instant::now() >= next_operation {
            if desired {
                let handle = resources.handle.clone();
                let guard = resources.authority.lan_guard();
                operation = Some(LanWireOperation::Subscribe(
                    async move {
                        handle
                            .subscribe(Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                            .await
                    }
                    .boxed_local(),
                ));
            } else if let Some(current) = subscription.clone() {
                let handle = resources.handle.clone();
                let guard = resources.authority.lan_guard();
                operation = Some(LanWireOperation::Unsubscribe(
                    async move {
                        handle
                            .unsubscribe(&current, Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                            .await
                    }
                    .boxed_local(),
                ));
            }
        }
        enum LanEvent {
            SessionStopped,
            Changed,
            Notification(Result<LanNotification, flume::RecvError>),
            Subscribed(Result<crate::xiaomi::lan::LanSubscription, crate::xiaomi::lan::LanError>),
            Unsubscribed(Result<(), crate::xiaomi::lan::LanError>),
            OperationDue,
        }
        let mut waits = Vec::<LocalBoxFuture<'_, LanEvent>>::new();
        waits.push(
            session
                .as_mut()
                .map(|_| LanEvent::SessionStopped)
                .boxed_local(),
        );
        waits.push(
            async {
                listener.await;
                LanEvent::Changed
            }
            .boxed_local(),
        );
        waits
            .push(async { LanEvent::Notification(notifications.recv_async().await) }.boxed_local());
        match operation.as_mut() {
            Some(LanWireOperation::Subscribe(operation)) => {
                waits.push(operation.as_mut().map(LanEvent::Subscribed).boxed_local());
            }
            Some(LanWireOperation::Unsubscribe(operation)) => {
                waits.push(operation.as_mut().map(LanEvent::Unsubscribed).boxed_local());
            }
            None if (desired || subscription.is_some()) && Instant::now() < next_operation => {
                waits.push(
                    async move {
                        Timer::at(next_operation).await;
                        LanEvent::OperationDue
                    }
                    .boxed_local(),
                );
            }
            None => {}
        }
        let (event, _, _) = select_all(waits).await;
        match event {
            LanEvent::SessionStopped => return false,
            LanEvent::Changed => {}
            LanEvent::OperationDue => {}
            LanEvent::Notification(Err(_)) => return false,
            LanEvent::Notification(Ok(LanNotification::SubscriptionHint { .. })) => {
                next_operation = Instant::now();
            }
            LanEvent::Notification(Ok(notification)) => {
                if let Some(token) = resources.token() {
                    apply_lan_state_notification(&resources.state, &token, notification);
                }
            }
            LanEvent::Subscribed(Ok(current)) => {
                operation = None;
                subscription = Some(current.clone());
                next_operation = if control.desired_push.get() {
                    Instant::now() + LAN_SUBSCRIPTION_RENEWAL
                } else {
                    Instant::now()
                };
                if control.desired_push.get()
                    && let Some(token) = resources.state.select_push_source(
                        device,
                        PushSource::Lan,
                        resources.handle.did(),
                        current.generation,
                    )
                {
                    resources
                        .state
                        .acknowledge(&token, resources.handle.did(), current.generation);
                    resources.replace_token(Some(token));
                    control.set_push_active(true);
                }
            }
            LanEvent::Subscribed(Err(error)) => {
                let _ = facts.try_send(LanConnectionFact::Failure {
                    stage: XiaomiFailureStage::Subscribe,
                    code: lan_failure_code(error.kind()),
                });
                operation = None;
                subscription = None;
                resources.replace_token(None);
                control.set_push_active(false);
                next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
            }
            LanEvent::Unsubscribed(result) => {
                operation = None;
                if result.is_ok() {
                    subscription = None;
                    resources.replace_token(None);
                    control.set_push_active(false);
                    next_operation = Instant::now();
                } else {
                    if let Err(error) = &result {
                        let _ = facts.try_send(LanConnectionFact::Failure {
                            stage: XiaomiFailureStage::Subscribe,
                            code: lan_failure_code(error.kind()),
                        });
                    }
                    next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
                }
            }
        }
    }
}

fn apply_lan_state_notification(
    state: &StateRuntime,
    token: &SubscriptionToken,
    notification: LanNotification,
) {
    match notification {
        LanNotification::Property {
            did,
            siid,
            piid,
            value,
            generation,
            ..
        } => {
            state.apply_property(
                token,
                did,
                generation,
                siid,
                piid,
                Some(&value),
                unix_time(),
                false,
            );
        }
        LanNotification::Event {
            did,
            siid,
            eiid,
            arguments,
            generation,
            ..
        } => match arguments {
            LanEventArguments::Keyed(arguments) => {
                let values = arguments
                    .into_iter()
                    .map(|argument| (argument.piid, argument.value))
                    .collect::<Vec<_>>();
                state.apply_keyed_event(
                    token,
                    did,
                    generation,
                    siid,
                    eiid,
                    &values,
                    unix_time(),
                    false,
                );
            }
            LanEventArguments::Positional(values) => {
                state.apply_positional_event(
                    token,
                    did,
                    generation,
                    siid,
                    eiid,
                    &values,
                    unix_time(),
                    false,
                );
            }
        },
        LanNotification::SubscriptionHint { .. } => {}
    }
}

fn cloud_connection_fact(
    receiver: flume::Receiver<CloudConnectionFact>,
) -> LocalBoxFuture<'static, CloudConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

fn active_cloud_connection_lifetime(
    handle: crate::xiaomi::cloud::CloudNotificationHandle,
    authority: SessionAuthority,
    notifications: flume::Receiver<CloudNotification>,
    mut session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    mut selected: BTreeSet<String>,
    control: CloudConnectionControl,
    facts: flume::Sender<CloudConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut observed_desired = selected.clone();
        let mut next_operation = Instant::now();
        let mut operation = None::<LocalBoxFuture<'static, CloudSelectionResult>>;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired != observed_desired {
                observed_desired = desired.clone();
                next_operation = Instant::now();
            }
            if operation.is_none() && desired != selected && Instant::now() >= next_operation {
                let operation_handle = handle.clone();
                let values = desired.iter().cloned().collect();
                let guard = authority.mqtt_guard();
                let result_desired = desired.clone();
                operation = Some(
                    async move {
                        CloudSelectionResult {
                            desired: result_desired,
                            result: operation_handle
                                .select_dids_guarded(
                                    values,
                                    Instant::now() + LOCAL_SESSION_TIMEOUT,
                                    guard,
                                )
                                .await,
                        }
                    }
                    .boxed_local(),
                );
            }
            enum Ready {
                Stopped,
                Changed,
                Notification(Result<CloudNotification, flume::RecvError>),
                Selection(CloudSelectionResult),
                Retry,
            }
            let listener = control.changed.listen();
            let mut waits = vec![
                session.as_mut().map(|_| Ready::Stopped).boxed_local(),
                async {
                    listener.await;
                    Ready::Changed
                }
                .boxed_local(),
                async { Ready::Notification(notifications.recv_async().await) }.boxed_local(),
            ];
            if let Some(current) = operation.as_mut() {
                waits.push(current.as_mut().map(Ready::Selection).boxed_local());
            } else if desired != selected && Instant::now() < next_operation {
                waits.push(
                    async {
                        Timer::at(next_operation).await;
                        Ready::Retry
                    }
                    .boxed_local(),
                );
            }
            let ready = {
                let (ready, _, _) = select_all(waits).await;
                ready
            };
            match ready {
                Ready::Stopped | Ready::Notification(Err(_)) => return,
                Ready::Changed | Ready::Retry => {}
                Ready::Notification(Ok(notification)) => {
                    if facts
                        .try_send(CloudConnectionFact::Notification(notification))
                        .is_err()
                    {
                        return;
                    }
                }
                Ready::Selection(completed) => {
                    operation = None;
                    if completed.result.is_ok() {
                        selected = completed.desired.clone();
                        next_operation = Instant::now();
                    } else {
                        next_operation = Instant::now() + CLOUD_NOTIFICATION_RETRY_INTERVAL;
                    }
                    if facts
                        .try_send(CloudConnectionFact::Selection(completed))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
    .boxed_local()
}

fn cloud_connection_lifetime(
    config: CloudConnectionConfig,
    authority: SessionAuthority,
    control: CloudConnectionControl,
    facts: flume::Sender<CloudConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut retry_delay = CLOUD_NOTIFICATION_RETRY_INTERVAL;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired.is_empty() {
                control.changed.listen().await;
                continue;
            }
            let Ok(tls) = CloudTlsConfig::webpki(CLOUD_MQTT_HOST) else {
                return;
            };
            let deadline = Instant::now() + LOCAL_SESSION_TIMEOUT;
            let startup = CloudNotificationStartup {
                host: CLOUD_MQTT_HOST.into(),
                port: CLOUD_MQTT_PORT,
                tls,
                oauth_client_uuid: config.oauth_client_uuid.clone(),
                access_token: config.access_token.clone(),
                keep_alive: Duration::from_secs(60),
                dids: desired.iter().cloned().collect(),
                deadline,
            };
            enum Startup {
                Started(Result<super::RunningCloudNotifications, TransportStartupError>),
                Changed,
            }
            let started = {
                let startup = start_cloud_notifications(startup);
                futures_lite::pin!(startup);
                loop {
                    let listener = control.changed.listen();
                    if control.stopped.get() {
                        break Startup::Changed;
                    }
                    match future::or(startup.as_mut().map(Startup::Started), async {
                        listener.await;
                        Startup::Changed
                    })
                    .await
                    {
                        Startup::Changed if !control.stopped.get() => continue,
                        value => break value,
                    }
                }
            };
            let running = match started {
                Startup::Changed => return,
                Startup::Started(Ok(running)) => running,
                Startup::Started(Err(error)) => {
                    let _ = facts.try_send(CloudConnectionFact::Failure {
                        stage: XiaomiFailureStage::Connect,
                        code: startup_failure_code(&error),
                    });
                    let delay = advance_retry(&mut retry_delay, CLOUD_NOTIFICATION_RETRY_INTERVAL);
                    let listener = control.changed.listen();
                    future::or(
                        async {
                            Timer::after(delay).await;
                        },
                        async {
                            listener.await;
                        },
                    )
                    .await;
                    continue;
                }
            };
            retry_delay = CLOUD_NOTIFICATION_RETRY_INTERVAL;
            let (handle, notifications, generation, task) = running.into_parts();
            if facts
                .try_send(CloudConnectionFact::Selection(CloudSelectionResult {
                    desired: desired.clone(),
                    result: Ok(generation),
                }))
                .is_err()
            {
                return;
            }
            active_cloud_connection_lifetime(
                handle,
                authority.clone(),
                notifications,
                task,
                desired,
                control.clone(),
                facts.clone(),
            )
            .await;
            if control.stopped.get() {
                return;
            }
            if facts.try_send(CloudConnectionFact::Disconnected).is_err() {
                return;
            }
            let delay = advance_retry(&mut retry_delay, CLOUD_NOTIFICATION_RETRY_INTERVAL);
            let listener = control.changed.listen();
            future::or(
                async {
                    Timer::after(delay).await;
                },
                async {
                    listener.await;
                },
            )
            .await;
        }
    }
    .boxed_local()
}

fn gateway_connection_fact(
    receiver: flume::Receiver<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, GatewayConnectionFactResult> {
    async move {
        let result = receiver.recv_async().await;
        (receiver, result)
    }
    .boxed_local()
}

fn active_gateway_connection_lifetime(
    did: u64,
    handle: crate::xiaomi::gateway::GatewayHandle,
    authority: SessionAuthority,
    notifications: flume::Receiver<GatewayNotification>,
    mut session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
    control: GatewayConnectionControl,
    facts: flume::Sender<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut selected = BTreeSet::new();
        let mut completed_refresh = control.refresh.get();
        let mut observed_desired = control.desired.borrow().clone();
        let mut next_operation = Instant::now();
        let mut operation = None::<LocalBoxFuture<'static, GatewayOperationResult>>;
        loop {
            if control.stopped.get() {
                return;
            }
            let desired = control.desired.borrow().clone();
            if desired != observed_desired {
                observed_desired = desired.clone();
                next_operation = Instant::now();
            }
            if operation.is_none() {
                if desired != selected && Instant::now() >= next_operation {
                    let values = desired.iter().cloned().collect();
                    let operation_handle = handle.clone();
                    let result_desired = desired.clone();
                    operation = Some(
                        async move {
                            let result = operation_handle
                                .select_notifications(
                                    values,
                                    Instant::now() + LOCAL_SESSION_TIMEOUT,
                                )
                                .await;
                            GatewayOperationResult::Selected {
                                did,
                                desired: result_desired,
                                result,
                            }
                        }
                        .boxed_local(),
                    );
                } else if desired == selected
                    && completed_refresh != control.refresh.get()
                    && Instant::now() >= next_operation
                {
                    let operation_handle = handle.clone();
                    let guard = authority.mqtt_guard();
                    operation = Some(
                        async move {
                            GatewayOperationResult::Refreshed {
                                did,
                                result: operation_handle
                                    .get_devices(Instant::now() + LOCAL_SESSION_TIMEOUT, guard)
                                    .await,
                            }
                        }
                        .boxed_local(),
                    );
                }
            }
            enum Ready {
                Stopped,
                Changed,
                Notification(Result<GatewayNotification, flume::RecvError>),
                Operation(GatewayOperationResult),
                Retry,
            }
            let listener = control.changed.listen();
            let mut waits = vec![
                session.as_mut().map(|_| Ready::Stopped).boxed_local(),
                async {
                    listener.await;
                    Ready::Changed
                }
                .boxed_local(),
                async { Ready::Notification(notifications.recv_async().await) }.boxed_local(),
            ];
            if let Some(current) = operation.as_mut() {
                waits.push(current.as_mut().map(Ready::Operation).boxed_local());
            } else if (desired != selected || completed_refresh != control.refresh.get())
                && Instant::now() < next_operation
            {
                waits.push(
                    async {
                        Timer::at(next_operation).await;
                        Ready::Retry
                    }
                    .boxed_local(),
                );
            }
            let ready = {
                let (ready, _, _) = select_all(waits).await;
                ready
            };
            match ready {
                Ready::Stopped | Ready::Notification(Err(_)) => return,
                Ready::Changed | Ready::Retry => {}
                Ready::Notification(Ok(notification)) => {
                    if facts
                        .try_send(GatewayConnectionFact::Notification(notification))
                        .is_err()
                    {
                        return;
                    }
                }
                Ready::Operation(result) => {
                    operation = None;
                    match &result {
                        GatewayOperationResult::Selected {
                            desired, result, ..
                        } if result.is_ok() => {
                            selected = desired.clone();
                            next_operation = Instant::now();
                        }
                        GatewayOperationResult::Refreshed { result, .. } if result.is_ok() => {
                            completed_refresh = control.refresh.get();
                            next_operation = Instant::now();
                        }
                        _ => {
                            next_operation = Instant::now() + LOCAL_RETRY_INTERVAL;
                        }
                    }
                    if facts
                        .try_send(GatewayConnectionFact::Operation(result))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
    .boxed_local()
}

fn gateway_full_connection_lifetime(
    config: GatewayConnectionConfig,
    control: GatewayConnectionControl,
    facts: flume::Sender<GatewayConnectionFact>,
) -> LocalBoxFuture<'static, ()> {
    async move {
        let mut endpoint_index = 0;
        let mut next_attempt = 1_u64;
        let mut retry_delay = LOCAL_RETRY_INTERVAL;
        loop {
            if control.stopped.get() {
                return;
            }
            let (endpoint, _interface) =
                config.endpoints[endpoint_index % config.endpoints.len()].clone();
            endpoint_index = endpoint_index.wrapping_add(1);
            let attempt = next_attempt;
            next_attempt = next_attempt.wrapping_add(1).max(1);
            let attempt_authority = config.authority.fresh_lease();
            let _attempt_scope = GatewayAttemptScope(attempt_authority.clone());
            let Some(setup_slot) = config.setup.acquire_gateway(&control).await else {
                return;
            };
            #[cfg(test)]
            let injected = config.startup.borrow_mut().take();
            #[cfg(test)]
            let startup = if let Some(startup) = injected {
                startup
            } else {
                let Ok(tls) = GatewayTlsConfig::new(
                    XIAOMI_CA_PEM,
                    &config.certificate_pem,
                    &config.private_key_pem,
                ) else {
                    return;
                };
                start_gateway(GatewayStartup {
                    endpoint: std::net::SocketAddrV4::new(endpoint.address, endpoint.port),
                    tls,
                    mqtt: MqttConfig::new(
                        config.virtual_did.clone(),
                        None,
                        Duration::from_secs(60),
                    ),
                    virtual_did: config.virtual_did.clone(),
                    gateway_did: config.candidate.gateway_did,
                    peer_did: config.candidate.gateway_did.to_string(),
                    epoch: config.network.epoch,
                    deadline: Instant::now() + LOCAL_SESSION_TIMEOUT,
                    guard: attempt_authority.mqtt_guard(),
                })
                .boxed_local()
            };
            #[cfg(not(test))]
            let startup = {
                let Ok(tls) = GatewayTlsConfig::new(
                    XIAOMI_CA_PEM,
                    &config.certificate_pem,
                    &config.private_key_pem,
                ) else {
                    return;
                };
                start_gateway(GatewayStartup {
                    endpoint: std::net::SocketAddrV4::new(endpoint.address, endpoint.port),
                    tls,
                    mqtt: MqttConfig::new(
                        config.virtual_did.clone(),
                        None,
                        Duration::from_secs(60),
                    ),
                    virtual_did: config.virtual_did.clone(),
                    gateway_did: config.candidate.gateway_did,
                    peer_did: config.candidate.gateway_did.to_string(),
                    epoch: config.network.epoch,
                    deadline: Instant::now() + LOCAL_SESSION_TIMEOUT,
                    guard: attempt_authority.mqtt_guard(),
                })
                .boxed_local()
            };
            enum Startup {
                Started(Result<super::RunningGateway, TransportStartupError>),
                Changed,
            }
            let started = {
                futures_lite::pin!(startup);
                loop {
                    let listener = control.changed.listen();
                    if control.stopped.get() {
                        break Startup::Changed;
                    }
                    match future::or(startup.as_mut().map(Startup::Started), async {
                        listener.await;
                        Startup::Changed
                    })
                    .await
                    {
                        Startup::Changed if !control.stopped.get() => continue,
                        value => break value,
                    }
                }
            };
            drop(setup_slot);
            let running = match started {
                Startup::Changed => return,
                Startup::Started(Ok(running)) => running,
                Startup::Started(Err(error)) => {
                    let _ = facts.try_send(GatewayConnectionFact::Failure {
                        stage: XiaomiFailureStage::Connect,
                        code: startup_failure_code(&error),
                    });
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
                    let listener = control.changed.listen();
                    future::or(
                        async {
                            Timer::after(delay).await;
                        },
                        async {
                            listener.await;
                        },
                    )
                    .await;
                    continue;
                }
            };
            retry_delay = LOCAL_RETRY_INTERVAL;
            let parts = running.into_parts();
            let ready = GatewayReady {
                attempt,
                endpoint: endpoint.clone(),
                handle: parts.handle.clone(),
                evidence: parts.evidence,
                authority: attempt_authority.clone(),
            };
            let mut session = parts.task;
            while control.publication.get() == GatewayPublication::Pending {
                let _ = facts.try_send(GatewayConnectionFact::Ready(Box::new(ready.clone())));
                let listener = control.changed.listen();
                enum Pending {
                    Session,
                    Changed,
                    Retry,
                }
                let (event, _, _) = select_all(vec![
                    session.as_mut().map(|_| Pending::Session).boxed_local(),
                    async {
                        listener.await;
                        Pending::Changed
                    }
                    .boxed_local(),
                    async {
                        Timer::after(AUTH_OBSERVE_INTERVAL).await;
                        Pending::Retry
                    }
                    .boxed_local(),
                ])
                .await;
                if matches!(event, Pending::Session) {
                    break;
                }
                if control.stopped.get() {
                    return;
                }
            }
            match control.publication.get() {
                GatewayPublication::Rejected => return,
                GatewayPublication::Pending => {
                    attempt_authority.revoke();
                    if facts
                        .send_async(GatewayConnectionFact::Disconnected { attempt })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
                    let listener = control.changed.listen();
                    future::or(
                        async {
                            Timer::after(delay).await;
                        },
                        async {
                            listener.await;
                        },
                    )
                    .await;
                    continue;
                }
                GatewayPublication::Active => {}
            }
            active_gateway_connection_lifetime(
                config.candidate.gateway_did,
                parts.handle,
                attempt_authority.clone(),
                parts.notifications,
                session,
                control.clone(),
                facts.clone(),
            )
            .await;
            attempt_authority.revoke();
            if control.stopped.get() {
                return;
            }
            control.publication.set(GatewayPublication::Pending);
            if facts
                .send_async(GatewayConnectionFact::Disconnected { attempt })
                .await
                .is_err()
            {
                return;
            }
            let delay = advance_retry(&mut retry_delay, LOCAL_RETRY_INTERVAL);
            Timer::after(delay).await;
        }
    }
    .boxed_local()
}

#[cfg(test)]
fn active_gateway(
    proof: AuthenticatedGateway,
    authority: SessionAuthority,
    parts: super::GatewayRuntimeParts,
) -> ActiveGateway {
    let did = parts.evidence.gateway_did;
    let control = GatewayConnectionControl::new();
    let (fact_sender, fact_receiver) = flume::bounded(256);
    ActiveGateway {
        did,
        handle: parts.handle.clone(),
        authority: authority.clone(),
        root_authority: authority.clone(),
        attempt: 1,
        wire_generation: 0,
        proof,
        task: active_gateway_connection_lifetime(
            did,
            parts.handle,
            authority,
            parts.notifications,
            parts.task,
            control.clone(),
            fact_sender,
        ),
        fact: gateway_connection_fact(fact_receiver),
        publication: None,
        control,
        desired_dids: BTreeSet::new(),
        selected_dids: BTreeSet::new(),
        refresh_pending: false,
        tokens: BTreeMap::new(),
    }
}

fn unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_secs().try_into().unwrap_or(i64::MAX)
        })
}

fn advance_retry(current: &mut Duration, initial: Duration) -> Duration {
    let delay = *current;
    *current = current
        .saturating_mul(2)
        .min(CONNECTION_RETRY_MAX)
        .max(initial);
    delay
}

fn startup_failure_code(error: &TransportStartupError) -> XiaomiSafeFailureCode {
    match error {
        TransportStartupError::Timeout => XiaomiSafeFailureCode::Timeout,
        TransportStartupError::Mqtt(error) => mqtt_failure_code(error.kind()),
        TransportStartupError::Gateway(error) => match error.kind() {
            crate::xiaomi::gateway::GatewayErrorKind::Protocol
            | crate::xiaomi::gateway::GatewayErrorKind::InvalidInput => {
                XiaomiSafeFailureCode::Protocol
            }
            crate::xiaomi::gateway::GatewayErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
            crate::xiaomi::gateway::GatewayErrorKind::Business(code) => {
                XiaomiSafeFailureCode::Rejected(*code)
            }
            crate::xiaomi::gateway::GatewayErrorKind::Transport => {
                XiaomiSafeFailureCode::Unavailable
            }
            crate::xiaomi::gateway::GatewayErrorKind::Superseded => {
                XiaomiSafeFailureCode::Unavailable
            }
        },
    }
}

fn mqtt_failure_code(kind: &crate::xiaomi::mqtt::MqttErrorKind) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::mqtt::MqttErrorKind::Unauthorized => XiaomiSafeFailureCode::Unauthorized,
        crate::xiaomi::mqtt::MqttErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::mqtt::MqttErrorKind::Protocol
        | crate::xiaomi::mqtt::MqttErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::mqtt::MqttErrorKind::Network
        | crate::xiaomi::mqtt::MqttErrorKind::Disconnected
        | crate::xiaomi::mqtt::MqttErrorKind::Capacity
        | crate::xiaomi::mqtt::MqttErrorKind::Superseded => XiaomiSafeFailureCode::Unavailable,
    }
}

fn gateway_failure_code(kind: &crate::xiaomi::gateway::GatewayErrorKind) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::gateway::GatewayErrorKind::Protocol
        | crate::xiaomi::gateway::GatewayErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::gateway::GatewayErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::gateway::GatewayErrorKind::Business(code) => {
            XiaomiSafeFailureCode::Rejected(*code)
        }
        crate::xiaomi::gateway::GatewayErrorKind::Transport
        | crate::xiaomi::gateway::GatewayErrorKind::Superseded => {
            XiaomiSafeFailureCode::Unavailable
        }
    }
}

fn lan_failure_code(kind: &crate::xiaomi::lan::LanErrorKind) -> XiaomiSafeFailureCode {
    match kind {
        crate::xiaomi::lan::LanErrorKind::Timeout => XiaomiSafeFailureCode::Timeout,
        crate::xiaomi::lan::LanErrorKind::Protocol
        | crate::xiaomi::lan::LanErrorKind::InvalidInput => XiaomiSafeFailureCode::Protocol,
        crate::xiaomi::lan::LanErrorKind::Business(code) => XiaomiSafeFailureCode::Rejected(*code),
        crate::xiaomi::lan::LanErrorKind::Transport
        | crate::xiaomi::lan::LanErrorKind::NotAuthenticated
        | crate::xiaomi::lan::LanErrorKind::Unsupported
        | crate::xiaomi::lan::LanErrorKind::RateLimited
        | crate::xiaomi::lan::LanErrorKind::Cancelled => XiaomiSafeFailureCode::Unavailable,
    }
}

fn auth_maintenance_deadline(snapshot: &AuthSnapshot) -> Instant {
    let Some(record) = snapshot.record.as_ref() else {
        return Instant::now() + AUTH_MAINTENANCE_INTERVAL;
    };
    let now = unix_time();
    let certificate_due = crate::xiaomi::certificate::validate_certificate(
        &record.uid,
        &record.virtual_did,
        &record.private_key_pem,
        &record.certificate_pem,
    )
    .map(|validity| validity.not_after.saturating_sub(3 * 24 * 60 * 60))
    .unwrap_or(now);
    let due = record.tokens.refresh_at.min(certificate_due);
    let delay = due.saturating_sub(now).max(0) as u64;
    Instant::now() + Duration::from_secs(delay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xiaomi::runtime::{
        AdmissionFeature, AdmissionSnapshot, AdmissionStatus, OperationPaths, RuntimeFeature,
    };
    use bytes::BytesMut;
    use futures_lite::future::{block_on, poll_once};
    use mqttbytes::{QoS, v5};
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    #[test]
    fn terminal_help_reads_the_current_runtime_auth_report_each_time() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let service = runtime.service();
        let render = || {
            block_on(crate::terminal::bridge::handle_line(
                &service,
                &store.devices(),
                &runtime,
                "/bin/migate".as_ref(),
                directory.path(),
                "help",
                0,
            ))
            .unwrap()
            .unwrap()
        };

        assert!(render().contains("Xiaomi: not signed in (cn)."));
        runtime.replace_auth_report(crate::xiaomi::auth::AuthReport::for_test(
            crate::xiaomi::auth::AuthenticationState::Checking,
            None,
            crate::xiaomi::auth::CertificateUpdate::NotNeeded,
            0,
        ));
        assert!(render().contains("Xiaomi: checking authentication (cn)..."));
    }

    #[test]
    fn manual_refresh_schedules_one_forced_state_read_per_admitted_device() {
        let mut response_index = 0;
        let (cloud_base, requests) =
            crate::xiaomi::test_support::dynamic_mock_server(2, move |request| {
                assert!(request.target.ends_with("/get"));
                let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
                let value = response_index != 0;
                response_index += 1;
                let result = body["params"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "did": item["did"],
                            "siid": item["siid"],
                            "piid": item["piid"],
                            "code": 0,
                            "value": value,
                        })
                    })
                    .collect::<Vec<_>>();
                crate::xiaomi::test_support::MockResponse::json(
                    200,
                    &serde_json::json!({"code": 0, "result": result}).to_string(),
                )
            });
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let service = runtime.service();
        let physical = crate::device::PhysicalDeviceId {
            account: crate::device::AccountId::new("account").unwrap(),
            home: crate::device::HomeId::new("home").unwrap(),
            parent_did: crate::device::DeviceDid::new("device").unwrap(),
        };
        let descriptors = crate::xiaomi::catalog::compile_spec(
            "xiaomi.switch.w3",
            include_str!("../../../tests/fixtures/miot_specs/xiaomi.switch.w3.json"),
        )
        .unwrap()
        .features;
        assert!(descriptors.len() > 1);
        let auth_generation = store.xiaomi().snapshot().unwrap().session_generation;
        let features = descriptors
            .into_iter()
            .take(2)
            .map(|descriptor| {
                let identity = crate::device::FeatureIdentity {
                    physical: physical.clone(),
                    service_instance: descriptor.service_instance,
                    role: descriptor.role,
                };
                service.publish(
                    identity.clone(),
                    descriptor.name.clone(),
                    descriptor.capabilities.clone(),
                );
                let registered = RuntimeFeature {
                    identity: identity.clone(),
                    descriptor,
                    authority_generation: 1,
                    auth_session_generation: auth_generation,
                };
                AdmissionFeature {
                    identity,
                    runtime: registered,
                    paths: OperationPaths {
                        cloud: true,
                        ..OperationPaths::default()
                    },
                    gateways: Vec::new(),
                    lan_evidence: None,
                }
            })
            .collect::<Vec<_>>();
        let snapshot = AdmissionSnapshot {
            binding: None,
            status: AdmissionStatus::Active,
            epoch: NetworkEpoch::new(1),
            features,
        };
        runtime.inner.registry.install_cloud(
            physical.clone(),
            Rc::new(CloudClient::for_test(&cloud_base, Duration::from_millis(300)).unwrap()),
            "access",
            unix_time() + 60,
            runtime.session_authority(),
        );
        runtime.inner.state.reconcile(&snapshot);
        block_on(runtime.inner.state.run_until_idle());
        let initial_requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(initial_requests.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initial_requests[0].body).unwrap()["params"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let token = runtime
            .inner
            .state
            .select_push_source(&physical, PushSource::Cloud, 1, 1)
            .unwrap();
        assert!(runtime.inner.state.acknowledge(&token, 1, 1));
        block_on(runtime.inner.state.run_until_idle());
        assert!(requests.try_recv().is_err());
        runtime.inner.status.borrow_mut().admission = snapshot.clone();

        runtime.refresh();
        runtime.refresh();
        block_on(runtime.inner.state.run_until_idle());

        let refresh_requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(refresh_requests.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&refresh_requests[0].body).unwrap()["params"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(runtime.inner.refresh_requested.get());
        for feature in &snapshot.features {
            assert!(matches!(
                service
                    .snapshot(&feature.identity)
                    .unwrap()
                    .property(crate::device::Property::Power),
                Some(crate::device::PropertyState::Current {
                    value: crate::device::PropertyValue::Power(true),
                    ..
                })
            ));
        }
    }

    fn mqtt_packet(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
        loop {
            match v5::read(buffer, 256 * 1024) {
                Ok(packet) => return packet,
                Err(mqttbytes::Error::InsufficientBytes(_)) => {
                    let mut bytes = [0; 4096];
                    let count = stream.read(&mut bytes).unwrap();
                    assert!(count > 0);
                    buffer.extend_from_slice(&bytes[..count]);
                }
                Err(error) => panic!("invalid MQTT packet: {error:?}"),
            }
        }
    }

    fn mqtt_write(stream: &mut TcpStream, packet: &v5::Packet) {
        let mut bytes = BytesMut::new();
        match packet {
            v5::Packet::SubAck(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::UnsubAck(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::PubRec(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::PubRel(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::PubComp(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::Publish(value) => {
                value.write(&mut bytes).unwrap();
            }
            v5::Packet::ConnAck(value) => bytes.extend_from_slice(&[
                0x20,
                0x03,
                value.session_present as u8,
                value.code as u8,
                0,
            ]),
            packet => panic!("unsupported MQTT test packet: {packet:?}"),
        }
        stream.write_all(&bytes).unwrap();
    }

    fn mqtt_packet_ignoring_ping(stream: &mut TcpStream, buffer: &mut BytesMut) -> v5::Packet {
        loop {
            let packet = mqtt_packet(stream, buffer);
            if matches!(packet, v5::Packet::PingReq) {
                stream.write_all(&[0xd0, 0x00]).unwrap();
                continue;
            }
            return packet;
        }
    }

    async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "condition exceeded its hard deadline"
            );
            Timer::after(Duration::from_millis(5)).await;
        }
    }

    fn receive_request(
        stream: &mut TcpStream,
        buffer: &mut BytesMut,
    ) -> crate::xiaomi::gateway::MipsEnvelope {
        let v5::Packet::Publish(publish) = mqtt_packet(stream, buffer) else {
            panic!("expected gateway request")
        };
        let envelope = crate::xiaomi::gateway::MipsEnvelope::decode(&publish.payload).unwrap();
        mqtt_write(stream, &v5::Packet::PubRec(v5::PubRec::new(publish.pkid)));
        assert!(matches!(
            mqtt_packet_ignoring_ping(stream, buffer),
            v5::Packet::PubRel(_)
        ));
        mqtt_write(stream, &v5::Packet::PubComp(v5::PubComp::new(publish.pkid)));
        envelope
    }

    fn reply(
        stream: &mut TcpStream,
        _buffer: &mut BytesMut,
        _packet_id: u16,
        topic: &str,
        mid: u32,
        payload: &str,
    ) {
        let payload = crate::xiaomi::gateway::MipsEnvelope {
            mid,
            return_topic: None,
            payload: payload.into(),
            from: Some("local".into()),
        }
        .encode()
        .unwrap();
        let mut publish = v5::Publish::new(topic, QoS::AtMostOnce, payload);
        publish.pkid = 0;
        mqtt_write(stream, &v5::Packet::Publish(publish));
    }

    enum CloudBrokerCommand {
        Acknowledge,
        Offline,
        Online,
        Property(bool),
        Stop,
    }

    struct CloudRuntimeHarness {
        handle: crate::xiaomi::cloud::CloudNotificationHandle,
        notifications: flume::Receiver<CloudNotification>,
        session: LocalBoxFuture<'static, Result<(), TransportStartupError>>,
        subscribed: std::sync::mpsc::Receiver<()>,
        commands: std::sync::mpsc::Sender<CloudBrokerCommand>,
        broker: thread::JoinHandle<()>,
    }

    fn real_cloud_runtime_connection(did: &str) -> CloudRuntimeHarness {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected = did.to_owned();
        let (subscribed, subscription) = std::sync::mpsc::sync_channel(1);
        let (commands, broker_commands) = std::sync::mpsc::channel();
        let broker = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = BytesMut::new();
            assert!(matches!(
                mqtt_packet(&mut stream, &mut buffer),
                v5::Packet::Connect(_)
            ));
            mqtt_write(
                &mut stream,
                &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
            );
            let v5::Packet::Subscribe(subscribe) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected cloud selection subscription")
            };
            assert_eq!(subscribe.filters.len(), 3);
            assert!(
                subscribe
                    .filters
                    .iter()
                    .all(|filter| filter.path.contains(&expected))
            );
            subscribed.send(()).unwrap();
            for command in broker_commands {
                match command {
                    CloudBrokerCommand::Acknowledge => mqtt_write(
                        &mut stream,
                        &v5::Packet::SubAck(v5::SubAck::new(
                            subscribe.pkid,
                            vec![v5::SubscribeReasonCode::QoS2; subscribe.filters.len()],
                        )),
                    ),
                    CloudBrokerCommand::Offline | CloudBrokerCommand::Online => {
                        let event = if matches!(command, CloudBrokerCommand::Online) {
                            "online"
                        } else {
                            "offline"
                        };
                        mqtt_write(
                            &mut stream,
                            &v5::Packet::Publish(v5::Publish::new(
                                format!("device/{expected}/state/change"),
                                QoS::AtMostOnce,
                                format!(r#"{{"device_id":"{expected}","event":"{event}"}}"#),
                            )),
                        );
                    }
                    CloudBrokerCommand::Property(value) => mqtt_write(
                        &mut stream,
                        &v5::Packet::Publish(v5::Publish::new(
                            format!("device/{expected}/up/properties_changed/2/1"),
                            QoS::AtMostOnce,
                            format!(
                                r#"{{"params":{{"did":"{expected}","siid":2,"piid":1,"value":{value}}}}}"#
                            ),
                        )),
                    ),
                    CloudBrokerCommand::Stop => break,
                }
            }
            let mut remaining = Vec::new();
            stream.read_to_end(&mut remaining).unwrap();
            buffer.extend_from_slice(&remaining);
            while !buffer.is_empty() {
                let packet = v5::read(&mut buffer, 256 * 1024).unwrap();
                assert!(
                    !matches!(packet, v5::Packet::Subscribe(_)),
                    "Cloud selection was sent more than once"
                );
            }
        });
        let (handle, notifications, live) = block_on(async {
            let (mqtt, session, handle, notifications) =
                crate::xiaomi::cloud::CloudNotificationSession::new(
                    "oauth",
                    "access",
                    Duration::from_secs(5),
                    &address.ip().to_string(),
                    address.port(),
                    None,
                )
                .unwrap();
            let live = future::or(
                async { mqtt.run().await.map_err(TransportStartupError::Mqtt) },
                async { session.run().await.map_err(TransportStartupError::Mqtt) },
            )
            .boxed_local();
            (handle, notifications, live)
        });
        CloudRuntimeHarness {
            handle,
            notifications,
            session: live,
            subscribed: subscription,
            commands,
            broker,
        }
    }

    #[test]
    fn network_refresh_keeps_existing_cloud_routes_without_admitting_new_devices() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
        let certificate = crate::xiaomi::test_support::sign_csr(
            &identity.csr_pem,
            unix_time() - 60,
            unix_time() + 30 * 24 * 60 * 60,
        )
        .unwrap();
        store
            .xiaomi()
            .replace(&crate::storage::XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: crate::storage::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: unix_time() + 30 * 24 * 60 * 60,
                    refresh_at: unix_time() + 20 * 24 * 60 * 60,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let auth = store.xiaomi().snapshot().unwrap();
        let document =
            include_str!("../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned();
        let compiled =
            crate::xiaomi::catalog::compile_spec("yeelink.light.ml9", &document).unwrap();
        let account = crate::device::AccountId::new("10001").unwrap();
        let mut catalog = super::super::AdmissionCatalog {
            account: account.clone(),
            session_generation: auth.session_generation,
            catalog: crate::xiaomi::catalog::DeviceCatalog {
                uid: "10001".into(),
                homes: vec![crate::xiaomi::catalog::CatalogHome {
                    id: "home".into(),
                    name: "Home".into(),
                    group_id: "0011223344556677".into(),
                    rooms: vec![],
                }],
                devices: vec![crate::xiaomi::catalog::CatalogDevice {
                    home_id: "home".into(),
                    room_id: None,
                    parent_did: "device.did".into(),
                    name: "Light".into(),
                    model: "yeelink.light.ml9".into(),
                    spec_type: Some(compiled.type_urn.clone()),
                    pid: Some(0),
                    token: None,
                    online: Some(true),
                    local_ip: None,
                    parent_id: None,
                    features: compiled.features.clone(),
                }],
            },
            specifications: HashMap::from([(compiled.type_urn.clone(), document)]),
        };
        let network = crate::xiaomi::discovery::NetworkSnapshot::select(
            vec![crate::xiaomi::discovery::InterfaceRecord {
                index: 1,
                name: "en-test".into(),
                address: "192.0.2.2".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: crate::xiaomi::discovery::LinkType::Ethernet,
            }],
            None,
        )
        .unwrap();
        let endpoint = GatewayEndpoint {
            interface_index: 1,
            source_address: "192.0.2.2".parse().unwrap(),
            address: "192.0.2.3".parse().unwrap(),
            port: 8883,
        };
        let proof = AuthenticatedGateway {
            account: account.clone(),
            session_generation: auth.session_generation,
            candidate: GatewayCandidate {
                gateway_did: 123,
                home_group: "0011223344556677".into(),
                endpoints: vec![endpoint.clone()],
                unverified: false,
            },
            selected_endpoint: endpoint,
            network: network.clone(),
            evidence: crate::xiaomi::gateway::GatewayEvidence {
                gateway_did: 123,
                peer_did: "123".into(),
                epoch: NetworkEpoch::new(7),
                devices: vec![crate::xiaomi::gateway::GatewayDevice {
                    did: "device.did".into(),
                    name: "Light".into(),
                    urn: compiled.type_urn.clone(),
                    model: "yeelink.light.ml9".into(),
                    online: Some(true),
                    spec_v2_access: Some(true),
                    push_available: Some(false),
                }],
            },
        };

        let feature;
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.admission.invalidate(NetworkEpoch::new(7));
            runner.admission.observe_gateway(proof, &catalog).unwrap();
            runner
                .admission
                .observe_cloud(&CloudEvidence {
                    account,
                    session_generation: auth.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            runner.admission.remove_gateway(123).unwrap();
            runner.catalog = Some(catalog.clone());
            runner.network_update = Some(NetworkUpdate {
                epoch: NetworkEpoch::new(7),
                snapshot: network.clone(),
            });
            runner.epoch = 7;
            runtime.sync_cloud_routes(runner).unwrap();
            let admitted = runner.admission.snapshot().unwrap();
            let light = admitted
                .features
                .iter()
                .find(|candidate| candidate.identity.role == crate::device::FeatureRole::Light)
                .unwrap();
            feature = light.identity.clone();
            assert!(light.paths.cloud);
            assert!(
                RuntimeTransports::new(runtime.inner.registry.clone())
                    .available_paths(&feature.physical)
                    .cloud
            );
            runtime.inner.state.reconcile(&admitted);
            runtime
                .inner
                .service
                .apply_report(crate::device::StateReport {
                    feature: feature.clone(),
                    report_version: runtime.inner.service.next_report_version(),
                    source: crate::device::StateSource::Cloud,
                    observed_at: unix_time(),
                    values: BTreeMap::from([(
                        crate::device::Property::Power,
                        crate::device::PropertyValue::Power(true),
                    )]),
                });
            runtime.inner.state.reconcile(&admitted);
            assert!(runtime.inner.service.is_available(&feature));

            runtime
                .finish_network_refresh(
                    runner,
                    Ok(NetworkUpdate {
                        epoch: NetworkEpoch::new(8),
                        snapshot: network,
                    }),
                )
                .unwrap();
            let refreshed = runner.admission.snapshot().unwrap();
            assert!(
                refreshed
                    .features
                    .iter()
                    .find(|candidate| candidate.identity == feature)
                    .unwrap()
                    .paths
                    .cloud
            );
            assert!(
                RuntimeTransports::new(runtime.inner.registry.clone())
                    .available_paths(&feature.physical)
                    .cloud
            );
            assert!(runtime.inner.service.is_available(&feature));

            let mut unproved = catalog.catalog.devices[0].clone();
            unproved.parent_did = "new-cloud-only".into();
            unproved.name = "New cloud-only light".into();
            catalog.catalog.devices.push(unproved);
            runner.admission.apply_complete_catalog(&catalog).unwrap();
        }
        assert!(
            runtime
                .service()
                .features()
                .iter()
                .all(
                    |candidate| candidate.identity.physical.parent_did.as_str() != "new-cloud-only"
                )
        );

        drop(runtime);
        let (cloud_base, _) = crate::xiaomi::test_support::dynamic_mock_server(10, |request| {
            assert!(request.target.ends_with("/get"));
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            let result = body["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let piid = item["piid"].as_u64().unwrap();
                    serde_json::json!({
                        "did":"device.did",
                        "siid":2,
                        "piid":piid,
                        "code":0,
                        "value":match piid {
                            1 => serde_json::json!(true),
                            2 => serde_json::json!(50),
                            3 => serde_json::json!(4000),
                            _ => serde_json::json!(0),
                        }
                    })
                })
                .collect::<Vec<_>>();
            crate::xiaomi::test_support::MockResponse::json(
                200,
                &serde_json::json!({"code":0,"result":result}).to_string(),
            )
        });
        let restored = XiaomiRuntime::new(store.clone()).unwrap();
        let owned = crate::xiaomi::cloud::OwnedCatalog {
            uid: "10001".into(),
            homes: vec![crate::xiaomi::cloud::OwnedHome {
                id: "home".into(),
                name: "Home".into(),
                group_id: "0011223344556677".into(),
                dids: vec!["device.did".into(), "new-cloud-only".into()],
                rooms: vec![],
            }],
            devices: vec![
                crate::xiaomi::cloud::CloudDevice {
                    did: "device.did".into(),
                    uid: Some("10001".into()),
                    name: "Light".into(),
                    model: "yeelink.light.ml9".into(),
                    spec_type: Some(compiled.type_urn.clone()),
                    pid: Some(0),
                    token: None,
                    online: Some(true),
                    local_ip: None,
                    parent_id: None,
                },
                crate::xiaomi::cloud::CloudDevice {
                    did: "new-cloud-only".into(),
                    uid: Some("10001".into()),
                    name: "New cloud-only light".into(),
                    model: "yeelink.light.ml9".into(),
                    spec_type: Some(compiled.type_urn),
                    pid: Some(0),
                    token: None,
                    online: Some(true),
                    local_ip: None,
                    parent_id: None,
                },
            ],
        };
        {
            let mut holder = restored.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.cloud =
                Rc::new(CloudClient::for_test(&cloud_base, Duration::from_millis(500)).unwrap());
            runner.catalog = Some(catalog.clone());
            restored
                .finish_catalog_refresh(
                    runner,
                    CatalogTaskResult::Owned {
                        snapshot: store.xiaomi().snapshot().unwrap(),
                        owned: Some(owned),
                    },
                )
                .unwrap();
        }
        let restored_feature = restored
            .service()
            .features()
            .into_iter()
            .find(|candidate| candidate.identity.role == crate::device::FeatureRole::Light)
            .unwrap()
            .identity;
        {
            let holder = restored.inner.runner.borrow();
            let runner = holder.as_ref().unwrap();
            let snapshot = runner.admission.snapshot().unwrap();
            let restored_admission = snapshot
                .features
                .iter()
                .find(|candidate| candidate.identity == restored_feature)
                .unwrap();
            assert!(restored_admission.paths.cloud);
            assert!(
                RuntimeTransports::new(restored.inner.registry.clone())
                    .available_paths(&restored_feature.physical)
                    .cloud
            );
        }
        block_on(future::or(
            async {
                restored.inner.state.run().await;
                panic!("state runtime stopped before the historical cloud read completed")
            },
            async {
                wait_until(Duration::from_secs(1), || {
                    restored
                        .service()
                        .snapshot(&restored_feature)
                        .and_then(|snapshot| {
                            snapshot.property(crate::device::Property::Power).cloned()
                        })
                        .is_some_and(|state| {
                            matches!(
                                state,
                                crate::device::PropertyState::Current {
                                    value: crate::device::PropertyValue::Power(true),
                                    ..
                                }
                            )
                        })
                })
                .await;
                restored.inner.state.stop();
            },
        ));
        {
            let mut holder = restored.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            restored
                .inner
                .registry
                .revoke_cloud(&restored_feature.physical);
            runner.cloud_routes.clear();
            restored
                .finish_catalog_refresh(
                    runner,
                    CatalogTaskResult::Resolved {
                        snapshot: store.xiaomi().snapshot().unwrap(),
                        result: Ok((catalog.catalog.clone(), catalog.specifications.clone())),
                    },
                )
                .unwrap();
            assert!(
                RuntimeTransports::new(restored.inner.registry.clone())
                    .available_paths(&restored_feature.physical)
                    .cloud
            );
        }
    }

    #[test]
    fn signed_out_runtime_remains_alive_until_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store).unwrap();
        block_on(async {
            let run = runtime.run();
            futures_lite::pin!(run);
            assert!(poll_once(run.as_mut()).await.is_none());
            Timer::after(Duration::from_millis(20)).await;
            assert!(poll_once(run.as_mut()).await.is_none());
            runtime.stop();
            future::or(async { run.await.unwrap() }, async {
                Timer::after(Duration::from_millis(500)).await;
                panic!("stopped Xiaomi runtime did not exit")
            })
            .await;
        });
    }

    #[test]
    fn connection_retries_back_off_to_a_bounded_maximum_and_reset_after_success() {
        let mut retry = Duration::from_secs(5);
        assert_eq!(
            advance_retry(&mut retry, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            advance_retry(&mut retry, Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        retry = Duration::from_secs(5);
        assert_eq!(
            advance_retry(&mut retry, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        retry = Duration::from_secs(5 * 60);
        assert_eq!(
            advance_retry(&mut retry, Duration::from_secs(5)),
            Duration::from_secs(5 * 60)
        );
        assert_eq!(retry, Duration::from_secs(5 * 60));
    }

    #[test]
    fn safe_diagnostics_preserve_full_width_protocol_business_codes() {
        for code in [i64::MIN, i64::MAX] {
            let gateway = crate::xiaomi::gateway::GatewayErrorKind::Business(code);
            let lan = crate::xiaomi::lan::LanErrorKind::Business(code);
            let startup = TransportStartupError::Gateway(
                crate::xiaomi::gateway::GatewayError::for_test(gateway.clone()),
            );
            for diagnostic in [
                gateway_failure_code(&gateway),
                lan_failure_code(&lan),
                startup_failure_code(&startup),
            ] {
                let XiaomiSafeFailureCode::Rejected(actual) = diagnostic else {
                    panic!("expected a protocol business rejection")
                };
                assert_eq!(i128::from(actual), i128::from(code));
            }
        }
    }

    #[test]
    fn rejected_cloud_selection_retires_the_old_ack_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let authority = runtime.session_authority();
        let control = CloudConnectionControl::new(BTreeSet::from(["device.did".into()]));
        let (_facts, receiver) = flume::bounded(1);
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.cloud_notifications = Some(ActiveCloudNotifications {
            authority,
            control,
            task: future::pending().boxed_local(),
            fact: cloud_connection_fact(receiver),
            desired: BTreeSet::from(["device.did".into()]),
            generation: 9,
            session_id: 9,
            tokens: BTreeMap::new(),
            publication: None,
        });

        runtime
            .finish_cloud_selection(
                runner,
                CloudSelectionResult {
                    desired: BTreeSet::from(["device.new".into()]),
                    result: Err(crate::xiaomi::mqtt::MqttError::guard_failed()),
                },
            )
            .unwrap();

        let cloud = runner.cloud_notifications.as_ref().unwrap();
        assert!(cloud.desired.is_empty());
        assert_eq!(cloud.generation, 0);
        assert_eq!(cloud.session_id, 0);
        assert!(
            runtime
                .status()
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    XiaomiRuntimeDiagnostic::Boundary(XiaomiBoundaryDiagnostic {
                        component: XiaomiRuntimeComponent::Cloud,
                        stage: XiaomiFailureStage::Subscribe,
                        code: XiaomiSafeFailureCode::Unavailable,
                        subject: None,
                    })
                ))
        );
    }

    #[test]
    fn superseded_cloud_selection_preserves_the_confirmed_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store).unwrap();
        let authority = runtime.session_authority();
        let control = CloudConnectionControl::new(BTreeSet::from(["device.new".into()]));
        let (_facts, receiver) = flume::bounded(1);
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.cloud_notifications = Some(ActiveCloudNotifications {
            authority,
            control,
            task: future::pending().boxed_local(),
            fact: cloud_connection_fact(receiver),
            desired: BTreeSet::from(["device.did".into()]),
            generation: 9,
            session_id: 9,
            tokens: BTreeMap::new(),
            publication: None,
        });

        runtime
            .finish_cloud_selection(
                runner,
                CloudSelectionResult {
                    desired: BTreeSet::from(["device.did".into()]),
                    result: Err(crate::xiaomi::mqtt::MqttError::superseded()),
                },
            )
            .unwrap();

        let cloud = runner.cloud_notifications.as_ref().unwrap();
        assert_eq!(cloud.desired, BTreeSet::from(["device.did".into()]));
        assert_eq!(cloud.generation, 9);
        assert_eq!(cloud.session_id, 9);
        assert!(runtime.status().diagnostics.is_empty());
    }

    #[test]
    fn an_auth_refresh_cas_is_rechecked_at_its_committed_revision() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
        let certificate = crate::xiaomi::test_support::sign_csr(
            &identity.csr_pem,
            unix_time() - 60,
            unix_time() + 30 * 24 * 60 * 60,
        )
        .unwrap();
        store
            .xiaomi()
            .replace(&crate::storage::XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: crate::storage::TokenSet {
                    access_token: "old-access".into(),
                    refresh_token: "old-refresh".into(),
                    expires_at: unix_time() + 1000,
                    refresh_at: unix_time() - 1,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let (base, requests) = crate::xiaomi::test_support::mock_server(vec![
            crate::xiaomi::test_support::MockResponse::json(
                200,
                r#"{"code":0,"result":{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":1000}}"#,
            ),
            crate::xiaomi::test_support::MockResponse::json(
                200,
                r#"{"code":0,"result":{"homelist":[{"uid":"10001","dids":[]}]}}"#,
            ),
            crate::xiaomi::test_support::MockResponse::json(
                200,
                r#"{"code":0,"result":{"homelist":[{"uid":"10001","dids":[]}]}}"#,
            ),
        ]);
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.auth = Rc::new(AuthService::new(
                store.xiaomi(),
                CloudClient::for_test(&base, Duration::from_millis(500)).unwrap(),
            ));
            runner.next_auth = Instant::now();
            runner.next_catalog = Instant::now() + Duration::from_secs(60);
            runner.next_network = Instant::now() + Duration::from_secs(60);
            runner.network = None;
            runtime.inner.refresh_requested.set(false);
        }
        block_on(future::or(
            async {
                future::or(async { runtime.run().await.unwrap() }, async {
                    loop {
                        let stored = store.xiaomi().load().unwrap().unwrap();
                        if stored.tokens.access_token == "fresh-access"
                            && runtime.auth_report().is_success()
                        {
                            break;
                        }
                        Timer::after(Duration::from_millis(10)).await;
                    }
                    runtime.stop();
                })
                .await;
            },
            async {
                Timer::after(Duration::from_secs(2)).await;
                panic!(
                    "runtime did not recheck the revision committed by auth refresh: report={:?}, stored={:?}, requests={:?}",
                    runtime.auth_report(),
                    store
                        .xiaomi()
                        .load()
                        .unwrap()
                        .map(|record| record.tokens.access_token),
                    requests
                        .try_iter()
                        .map(|request| request.target)
                        .collect::<Vec<_>>()
                )
            },
        ));
        assert!(runtime.auth_report().is_success());
        assert_eq!(
            store.xiaomi().load().unwrap().unwrap().tokens.access_token,
            "fresh-access"
        );
    }

    #[test]
    fn a_late_old_session_auth_response_cannot_replace_a_new_login() {
        fn auth_record(uid: &str, access: &str, refresh_at: i64) -> crate::storage::XiaomiRecord {
            let identity = crate::xiaomi::certificate::ClientIdentity::generate(uid).unwrap();
            let certificate = crate::xiaomi::test_support::sign_csr(
                &identity.csr_pem,
                unix_time() - 60,
                unix_time() + 30 * 24 * 60 * 60,
            )
            .unwrap();
            crate::storage::XiaomiRecord {
                uid: uid.into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: crate::storage::TokenSet {
                    access_token: access.into(),
                    refresh_token: format!("{uid}-refresh"),
                    expires_at: unix_time() + 1000,
                    refresh_at,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store
            .xiaomi()
            .replace(&auth_record("10001", "old-access", unix_time() - 1))
            .unwrap();
        let (base, requests) = crate::xiaomi::test_support::mock_server(vec![
            crate::xiaomi::test_support::MockResponse::json(
                200,
                r#"{"code":0,"result":{"access_token":"late-old-access","refresh_token":"late-old-refresh","expires_in":1000}}"#,
            )
            .delayed(Duration::from_millis(200)),
            crate::xiaomi::test_support::MockResponse::json(
                200,
                r#"{"code":0,"result":{"homelist":[{"uid":"20002","dids":[]}]}}"#,
            ),
        ]);
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.auth = Rc::new(AuthService::new(
                store.xiaomi(),
                CloudClient::for_test(&base, Duration::from_millis(500)).unwrap(),
            ));
            runner.next_auth = Instant::now();
            runner.next_catalog = Instant::now() + Duration::from_secs(60);
            runner.next_network = Instant::now() + Duration::from_secs(60);
            runner.network = None;
            runtime.inner.refresh_requested.set(false);
        }
        let assertions_completed = Rc::new(Cell::new(false));
        let completed = assertions_completed.clone();
        block_on(future::or(
            async {
                future::or(async { runtime.run().await.unwrap() }, async {
                    blocking::unblock(move || {
                        requests.recv_timeout(Duration::from_secs(1)).unwrap()
                    })
                    .await;
                    let other = Store::open(directory.path()).unwrap();
                    other.xiaomi().logout().unwrap();
                    other
                        .xiaomi()
                        .replace(&auth_record("20002", "new-access", unix_time() + 500))
                        .unwrap();
                    loop {
                        let current = store.xiaomi().load().unwrap().unwrap();
                        if current.uid == "20002"
                            && current.tokens.access_token == "new-access"
                            && runtime.auth_report().is_success()
                        {
                            break;
                        }
                        Timer::after(Duration::from_millis(10)).await;
                    }
                    Timer::after(Duration::from_millis(250)).await;
                    let current = store.xiaomi().load().unwrap().unwrap();
                    assert_eq!(current.uid, "20002");
                    assert_eq!(current.tokens.access_token, "new-access");
                    completed.set(true);
                    runtime.stop();
                })
                .await;
            },
            async {
                Timer::after(Duration::from_secs(2)).await;
                panic!("late old-session auth response was not isolated")
            },
        ));
        assert!(
            assertions_completed.get(),
            "runtime stopped before the stale auth assertions completed"
        );
    }

    #[test]
    fn cloud_connection_lifetime_switches_the_actual_wire_allowlist_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (first_seen, first_ready) = flume::bounded(1);
        let (release_first, release) = flume::bounded(1);
        let broker = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut buffer = BytesMut::new();
            assert!(matches!(
                mqtt_packet(&mut stream, &mut buffer),
                v5::Packet::Connect(_)
            ));
            mqtt_write(
                &mut stream,
                &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
            );
            let v5::Packet::Subscribe(first) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected first cloud allowlist subscription")
            };
            assert_eq!(first.filters.len(), 3);
            assert!(
                first
                    .filters
                    .iter()
                    .all(|filter| filter.path.contains("device.a"))
            );
            first_seen.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(3)).unwrap();
            mqtt_write(
                &mut stream,
                &v5::Packet::SubAck(v5::SubAck::new(
                    first.pkid,
                    vec![v5::SubscribeReasonCode::QoS2; first.filters.len()],
                )),
            );
            for _ in 0..3 {
                let v5::Packet::Unsubscribe(removed) = mqtt_packet(&mut stream, &mut buffer) else {
                    panic!("expected old cloud allowlist removal")
                };
                assert_eq!(removed.filters.len(), 1);
                assert!(removed.filters[0].contains("device.a"));
                let mut unsuback = v5::UnsubAck::new(removed.pkid);
                unsuback.reasons = vec![v5::UnsubAckReason::Success];
                mqtt_write(&mut stream, &v5::Packet::UnsubAck(unsuback));
            }
            let v5::Packet::Subscribe(second) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected replacement cloud allowlist subscription")
            };
            assert_eq!(second.filters.len(), 3);
            assert!(
                second
                    .filters
                    .iter()
                    .all(|filter| filter.path.contains("device.b"))
            );
            mqtt_write(
                &mut stream,
                &v5::Packet::SubAck(v5::SubAck::new(
                    second.pkid,
                    vec![v5::SubscribeReasonCode::QoS2; second.filters.len()],
                )),
            );
            let mut closed = [0; 1];
            assert_eq!(stream.read(&mut closed).unwrap(), 0);
        });

        block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
            let certificate = crate::xiaomi::test_support::sign_csr(
                &identity.csr_pem,
                unix_time() - 60,
                unix_time() + 30 * 24 * 60 * 60,
            )
            .unwrap();
            store
                .xiaomi()
                .replace(&crate::storage::XiaomiRecord {
                        uid: "10001".into(),
                        region: "cn".into(),
                    oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                    redirect_uri: "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef".into(),
                    tokens: crate::storage::TokenSet {
                        access_token: "access".into(),
                        refresh_token: "refresh".into(),
                        expires_at: unix_time() + 30 * 24 * 60 * 60,
                        refresh_at: unix_time() + 20 * 24 * 60 * 60,
                    },
                    virtual_did: identity.virtual_did,
                    private_key_pem: identity.private_key_pem,
                    certificate_pem: certificate,
                })
                .unwrap();
            let authority = SessionAuthority::new();
            let (mqtt, session, handle, notifications) =
                crate::xiaomi::cloud::CloudNotificationSession::new(
                    "oauth",
                    "access",
                    Duration::from_secs(5),
                    &address.ip().to_string(),
                    address.port(),
                    None,
                )
                .unwrap();
            let live = future::or(
                async { mqtt.run().await.map_err(TransportStartupError::Mqtt) },
                async { session.run().await.map_err(TransportStartupError::Mqtt) },
            )
            .boxed_local();
            let control = CloudConnectionControl::new(BTreeSet::from(["device.a".into()]));
            let (facts, fact_receiver) = flume::bounded(8);
            let lifetime = active_cloud_connection_lifetime(
                handle,
                authority,
                notifications,
                live,
                BTreeSet::new(),
                control.clone(),
                facts,
            );
            let application = async {
                first_ready.recv_async().await.unwrap();
                control.set_desired(BTreeSet::from(["device.b".into()]));
                release_first.send_async(()).await.unwrap();
                let first = fact_receiver.recv_async().await.unwrap();
                let CloudConnectionFact::Selection(first) = first else {
                    panic!("expected first cloud selection ACK")
                };
                assert_eq!(first.desired, BTreeSet::from(["device.a".into()]));
                assert!(first.result.is_ok());
                let second = fact_receiver.recv_async().await.unwrap();
                let CloudConnectionFact::Selection(second) = second else {
                    panic!("expected replacement cloud selection ACK")
                };
                assert_eq!(second.desired, BTreeSet::from(["device.b".into()]));
                assert!(second.result.is_ok());
                control.stop();
            };
            let completed = future::or(
                async {
                    future::zip(lifetime, application).await;
                    true
                },
                async {
                    Timer::after(Duration::from_secs(3)).await;
                    false
                },
            )
            .await;
            assert!(completed, "cloud wire source switch did not finish");
        });
        broker.join().unwrap();
    }

    #[test]
    fn gateway_ready_from_a_disconnected_pending_attempt_cannot_be_published() {
        exercise_gateway_disconnect(false);
    }

    #[test]
    fn active_gateway_disconnect_revokes_before_publication_cleanup() {
        exercise_gateway_disconnect(true);
    }

    fn exercise_gateway_disconnect(active: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (disconnect, disconnected) = std::sync::mpsc::sync_channel(1);
        let broker = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut buffer = BytesMut::new();
            assert!(matches!(
                mqtt_packet(&mut stream, &mut buffer),
                v5::Packet::Connect(_)
            ));
            mqtt_write(
                &mut stream,
                &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
            );
            let v5::Packet::Subscribe(initial) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected gateway base subscription")
            };
            mqtt_write(
                &mut stream,
                &v5::Packet::SubAck(v5::SubAck::new(
                    initial.pkid,
                    vec![v5::SubscribeReasonCode::QoS2; initial.filters.len()],
                )),
            );
            let devices = receive_request(&mut stream, &mut buffer);
            reply(
                &mut stream,
                &mut buffer,
                41,
                devices.return_topic.as_deref().unwrap(),
                devices.mid,
                r#"{"devList":{}}"#,
            );
            disconnected.recv_timeout(Duration::from_secs(3)).unwrap();
        });

        block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
            let certificate = crate::xiaomi::test_support::sign_csr(
                &identity.csr_pem,
                unix_time() - 60,
                unix_time() + 30 * 24 * 60 * 60,
            )
            .unwrap();
            store
                .xiaomi()
                .replace(&crate::storage::XiaomiRecord {
                        uid: "10001".into(),
                        region: "cn".into(),
                    oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                    redirect_uri: "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef".into(),
                    tokens: crate::storage::TokenSet {
                        access_token: "access".into(),
                        refresh_token: "refresh".into(),
                        expires_at: unix_time() + 30 * 24 * 60 * 60,
                        refresh_at: unix_time() + 20 * 24 * 60 * 60,
                    },
                    virtual_did: identity.virtual_did.clone(),
                    private_key_pem: identity.private_key_pem,
                    certificate_pem: certificate,
                })
                .unwrap();
            let authority = SessionAuthority::new();
            let startup_authority = authority.clone();
            let startup = async move {
                crate::xiaomi::runtime::start_gateway_plain_for_test(
                    match address {
                        std::net::SocketAddr::V4(value) => value,
                        _ => unreachable!(),
                    },
                    MqttConfig::new("runtime", None, Duration::from_secs(5)),
                    &identity.virtual_did,
                    123,
                    "192.0.2.10",
                    NetworkEpoch::new(7),
                    startup_authority.mqtt_guard(),
                    Instant::now() + Duration::from_secs(2),
                )
                .await
            }
            .boxed_local();
            let network = NetworkUpdate {
                epoch: NetworkEpoch::new(7),
                snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
                    vec![crate::xiaomi::discovery::InterfaceRecord {
                        index: 7,
                        name: "en-test".into(),
                        address: "192.0.2.2".parse().unwrap(),
                        netmask: "255.255.255.0".parse().unwrap(),
                        up: true,
                        point_to_point: false,
                        loopback: false,
                        link_type: crate::xiaomi::discovery::LinkType::Ethernet,
                    }],
                    None,
                )
                .unwrap(),
            };
            let endpoint = GatewayEndpoint {
                interface_index: 7,
                source_address: "192.0.2.2".parse().unwrap(),
                address: "192.0.2.10".parse().unwrap(),
                port: 8883,
            };
            let interface = network.snapshot.interfaces()[0].clone();
            let control = GatewayConnectionControl::new();
            let (facts, fact_receiver) = flume::bounded(8);
            let lifetime = gateway_full_connection_lifetime(
                GatewayConnectionConfig {
                    candidate: GatewayCandidate {
                        gateway_did: 123,
                        home_group: "group".into(),
                        endpoints: vec![endpoint.clone()],
                        unverified: false,
                    },
                    endpoints: vec![(endpoint, interface)],
                    network,
                    virtual_did: "unused".into(),
                    private_key_pem: String::new(),
                    certificate_pem: String::new(),
                    authority,
                    setup: LanSetupSlots::new(4),
                    startup: RefCell::new(Some(startup)),
                },
                control.clone(),
                facts,
            );
            let completed = future::or(
                async {
                    let application = async {
                        let mut ready = None;
                        loop {
                            match fact_receiver.recv_async().await.unwrap() {
                                GatewayConnectionFact::Ready(current) => {
                                    if ready.is_none() {
                                        ready = Some(current);
                                        if active {
                                            control.publish(GatewayPublication::Active);
                                        }
                                        disconnect.send(()).unwrap();
                                    }
                                }
                                GatewayConnectionFact::Disconnected { attempt } => {
                                    assert_eq!(attempt, 1);
                                    assert!(!ready.as_ref().unwrap().authority.check());
                                    control.stop();
                                    return ready;
                                }
                                _ => {}
                            }
                        }
                    };
                    future::or(application, async {
                        lifetime.await;
                        panic!("gateway session stopped before disconnect evidence")
                    })
                    .await
                },
                async {
                    Timer::after(Duration::from_secs(3)).await;
                    None
                },
            )
            .await;
            let Some(ready) = completed else {
                panic!("gateway attempt did not publish disconnect evidence")
            };
            assert_eq!(ready.attempt, 1);
            assert!(!ready.authority.check());
        });
        broker.join().unwrap();
    }

    #[test]
    fn dropping_run_revokes_the_runtime_scope() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store).unwrap();
        block_on(async {
            let mut run = Box::pin(runtime.run());
            assert!(poll_once(run.as_mut()).await.is_none());
            drop(run);
        });
        assert!(!runtime.inner.running.get());
        assert!(runtime.inner.stopped.get());
    }

    #[test]
    fn authenticated_catalog_without_a_hub_does_not_starve_stop() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let runtime = XiaomiRuntime::new(store).unwrap();
        let snapshot = crate::xiaomi::discovery::NetworkSnapshot::select(
            vec![crate::xiaomi::discovery::InterfaceRecord {
                index: 7,
                name: "en-test".into(),
                address: "192.0.2.2".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: crate::xiaomi::discovery::LinkType::Ethernet,
            }],
            None,
        )
        .unwrap();
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            let auth = runner.observer.snapshot().unwrap();
            runner.catalog = Some(crate::xiaomi::runtime::AdmissionCatalog {
                account: crate::device::AccountId::new("signed-out-placeholder").unwrap(),
                session_generation: auth.session_generation,
                catalog: crate::xiaomi::catalog::DeviceCatalog {
                    uid: "signed-out-placeholder".into(),
                    homes: vec![],
                    devices: vec![],
                },
                specifications: HashMap::new(),
            });
            runner.network_update = Some(NetworkUpdate {
                epoch: NetworkEpoch::new(1),
                snapshot: snapshot.clone(),
            });
            runner.discovery = Some(DiscoveryRegistry::new(NetworkEpoch::new(1), snapshot));
            runner.next_network = Instant::now() + Duration::from_secs(60);
            runner.next_auth = Instant::now() + Duration::from_secs(60);
            runner.next_catalog = Instant::now() + Duration::from_secs(60);
            runtime.inner.refresh_requested.set(false);
        }
        block_on(async {
            let completed = future::or(
                async {
                    runtime.run().await.unwrap();
                    true
                },
                async {
                    Timer::after(Duration::from_millis(30)).await;
                    runtime.stop();
                    Timer::after(Duration::from_millis(470)).await;
                    false
                },
            )
            .await;
            assert!(completed, "empty gateway retry work starved runtime stop");
        });
    }

    #[test]
    fn authoritative_owned_catalog_revokes_routes_before_spec_resolution() {
        exercise_catalog_and_subscription_completion(false, false, false, false);
    }

    #[test]
    fn rejected_gateway_push_subscription_keeps_gateway_control_alive() {
        exercise_catalog_and_subscription_completion(true, false, false, false);
    }

    #[test]
    fn coordinator_policy_retracts_a_timed_out_lan_route_before_cloud_fallback() {
        exercise_catalog_and_subscription_completion(false, true, false, false);
    }

    #[test]
    fn refreshed_gateway_admission_publishes_the_cloud_fallback_before_state() {
        exercise_catalog_and_subscription_completion(false, false, true, false);
    }

    #[test]
    fn cloud_coordinator_restores_online_state_on_the_same_mqtt_session() {
        exercise_catalog_and_subscription_completion(false, false, false, true);
    }

    #[test]
    fn runtime_run_owns_the_lan_session_through_admission_state_and_control() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
        let certificate = crate::xiaomi::test_support::sign_csr(
            &identity.csr_pem,
            unix_time() - 60,
            unix_time() + 30 * 24 * 60 * 60,
        )
        .unwrap();
        store
            .xiaomi()
            .replace(&crate::storage::XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: crate::storage::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: unix_time() + 30 * 24 * 60 * 60,
                    refresh_at: unix_time() + 20 * 24 * 60 * 60,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let account = crate::device::AccountId::new("10001").unwrap();
        let home = crate::device::HomeId::new("home").unwrap();
        let auth = store.xiaomi().snapshot().unwrap();
        assert!(
            store
                .devices()
                .publish_topology(&crate::storage::PublishedTopologyDelta {
                    account: account.clone(),
                    session_generation: auth.session_generation,
                    binding: Some(crate::storage::HomeBinding {
                        account: account.clone(),
                        home: home.clone(),
                        display_name: "Home".into(),
                    }),
                    devices: vec![],
                    definitions: vec![],
                    deactivate: vec![],
                })
                .unwrap()
                .is_some()
        );
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let (session, handle, notifications, lan_device, token) =
            crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
        let address = match lan_device.get_ref().local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!(),
        };
        let interface = crate::xiaomi::discovery::NetworkInterface {
            index: 1,
            name: "en-test".into(),
            address: "192.0.2.2".parse().unwrap(),
            netmask: "0.0.0.0".parse().unwrap(),
            prefix_len: 0,
        };
        let target = crate::xiaomi::lan::LanTarget::for_test(
            42,
            "lumi.acpartner.mcn02",
            address,
            interface.clone(),
            NetworkEpoch::new(7),
            token,
        );
        let type_urn = "urn:miot-spec-v2:device:air-conditioner:0000A004:lumi-mcn02:1";
        let spec = include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json");
        let owned = crate::xiaomi::cloud::OwnedCatalog {
            uid: "10001".into(),
            homes: vec![crate::xiaomi::cloud::OwnedHome {
                id: "home".into(),
                name: "Home".into(),
                group_id: "group".into(),
                dids: vec!["42".into()],
                rooms: vec![],
            }],
            devices: vec![crate::xiaomi::cloud::CloudDevice {
                did: "42".into(),
                uid: Some("10001".into()),
                name: "Air conditioner".into(),
                model: "lumi.acpartner.mcn02".into(),
                spec_type: Some(type_urn.into()),
                pid: Some(0),
                token: Some(crate::storage::DeviceToken(token.to_vec())),
                online: Some(true),
                local_ip: Some(address.ip().to_string()),
                parent_id: None,
            }],
        };
        let specifications = HashMap::from([(type_urn.to_owned(), spec.to_owned())]);
        let catalog = super::super::AdmissionCatalog {
            account: account.clone(),
            session_generation: auth.session_generation,
            catalog: assemble_catalog(&owned, &specifications).unwrap(),
            specifications,
        };
        let descriptor = catalog.catalog.devices[0]
            .features
            .iter()
            .find(|feature| feature.role == crate::device::FeatureRole::Climate)
            .unwrap()
            .clone();
        let property = descriptor
            .properties
            .iter()
            .find(|property| property.readable)
            .map(|property| LanProperty {
                siid: property.siid,
                piid: property.piid,
            })
            .unwrap();
        let network = NetworkUpdate {
            epoch: NetworkEpoch::new(7),
            snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
                vec![crate::xiaomi::discovery::InterfaceRecord {
                    index: 1,
                    name: "en-test".into(),
                    address: "192.0.2.2".parse().unwrap(),
                    netmask: "0.0.0.0".parse().unwrap(),
                    up: true,
                    point_to_point: false,
                    loopback: false,
                    link_type: crate::xiaomi::discovery::LinkType::Ethernet,
                }],
                None,
            )
            .unwrap(),
        };
        let startup = async move {
            crate::xiaomi::runtime::start_lan_session_for_test(
                session,
                handle,
                notifications,
                property,
                Instant::now() + Duration::from_secs(1),
                crate::xiaomi::lan::LanSendGuard::new(),
            )
            .await
        }
        .boxed_local();
        let lifecycle_control;
        {
            let mut holder = runtime.inner.runner.borrow_mut();
            let runner = holder.as_mut().unwrap();
            runner.admission.invalidate(NetworkEpoch::new(7));
            runner.catalog = Some(catalog.clone());
            runner.network_update = Some(network.clone());
            runner
                .admission
                .observe_cloud(&CloudEvidence {
                    account: account.clone(),
                    session_generation: auth.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            runner.next_network = Instant::now() + Duration::from_secs(60);
            runner.next_auth = Instant::now() + Duration::from_secs(60);
            runner.next_catalog = Instant::now() + Duration::from_secs(60);
            runner.next_browser_start = Instant::now() + Duration::from_secs(60);
            runtime.inner.refresh_requested.set(false);
            let control = LanConnectionControl::new(runtime.inner.wake.clone());
            lifecycle_control = control.clone();
            let (fact_sender, fact_receiver) = flume::bounded(2);
            let physical = crate::device::PhysicalDeviceId {
                account: account.clone(),
                home,
                parent_did: crate::device::DeviceDid::new("42").unwrap(),
            };
            let config = LanConnectionConfig {
                physical: physical.clone(),
                targets: vec![target.clone()],
                network,
                account,
                session_generation: auth.session_generation,
                descriptor,
                property,
                virtual_did: auth.record.as_ref().unwrap().virtual_did.parse().unwrap(),
                setup: runner.lan_setup.clone(),
                startup: RefCell::new(Some(startup)),
            };
            runner.lans.insert(
                physical,
                ActiveLan {
                    targets: vec![target],
                    control: control.clone(),
                    current: None,
                    task: lan_connection_lifetime(
                        config,
                        control,
                        fact_sender,
                        runtime.inner.registry.clone(),
                        runtime.inner.state.clone(),
                    ),
                    fact: lan_connection_fact(fact_receiver),
                },
            );
        }
        let application_done = Rc::new(Cell::new(false));
        let application_completed = application_done.clone();
        let wire_stage = Rc::new(Cell::new(0_u8));
        let simulated_stage = wire_stage.clone();
        let subscribe_count = Rc::new(Cell::new(0_u8));
        let simulated_subscribe_count = subscribe_count.clone();
        let unsubscribe_count = Rc::new(Cell::new(0_u8));
        let simulated_unsubscribe_count = unsubscribe_count.clone();
        let application = async {
            let feature = loop {
                if let Some(feature) =
                    runtime.service().features().into_iter().find(|feature| {
                        feature.identity.role == crate::device::FeatureRole::Climate
                    })
                {
                    break feature.identity;
                }
                Timer::after(Duration::from_millis(10)).await;
            };
            assert!(
                RuntimeTransports::new(runtime.inner.registry.clone())
                    .available_paths(&feature.physical)
                    .cloud,
                "LAN admission did not publish the authorized Cloud fallback"
            );
            loop {
                let current = runtime
                    .service()
                    .snapshot(&feature)
                    .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
                    .is_some_and(|state| {
                        matches!(
                            state,
                            crate::device::PropertyState::Current {
                                value: crate::device::PropertyValue::Power(true),
                                ..
                            }
                        )
                    });
                if current {
                    break;
                }
                Timer::after(Duration::from_millis(10)).await;
            }
            while !lifecycle_control.push_active.get() {
                Timer::after(Duration::from_millis(5)).await;
            }
            let accepted = runtime
                .service()
                .command(&feature, crate::device::DeviceCommand::SetPower(false))
                .await;
            assert_eq!(accepted, crate::device::CommandOutcome::Accepted);
            lifecycle_control.desired_push.set(false);
            lifecycle_control.changed.notify(usize::MAX);
            while lifecycle_control.push_active.get() {
                Timer::after(Duration::from_millis(5)).await;
            }
            application_completed.set(true);
            runtime.stop();
        };
        let simulated_device = async {
            let mut accepted_control = false;
            let mut unsubscribed = false;
            while !accepted_control || !unsubscribed {
                let (request, source) =
                    crate::xiaomi::lan::receive_request_for_test(&lan_device, &token).await;
                simulated_stage.set(simulated_stage.get().saturating_add(1));
                let method = request["method"].as_str().unwrap();
                if method == "miIO.sub" {
                    simulated_subscribe_count
                        .set(simulated_subscribe_count.get().saturating_add(1));
                    Timer::after(Duration::from_millis(20)).await;
                } else if method == "miIO.unsub" {
                    simulated_unsubscribe_count
                        .set(simulated_unsubscribe_count.get().saturating_add(1));
                    unsubscribed = true;
                }
                let response = match method {
                    "get_properties" => serde_json::json!({
                        "id":request["id"],"error":{"code":-1}
                    }),
                    "get_prop" => serde_json::json!({
                        "id":request["id"],
                        "result":["on","cool",25,"small_fan","off"]
                    }),
                    "miIO.sub" => serde_json::json!({
                        "id":request["id"],"result":{"code":0}
                    }),
                    "miIO.unsub" => serde_json::json!({
                        "id":request["id"],"result":{"code":0}
                    }),
                    "set_power" => {
                        assert_eq!(request["params"], serde_json::json!(["off"]));
                        accepted_control = true;
                        serde_json::json!({"id":request["id"],"result":["ok"]})
                    }
                    method => panic!("unexpected LAN method {method}"),
                };
                crate::xiaomi::lan::reply_for_test(&lan_device, &token, source, 100, response)
                    .await;
            }
        };
        block_on(future::race(
            async {
                future::or(
                    async {
                        let result = runtime.run().await;
                        if !application_done.get() {
                            panic!("XiaomiRuntime returned before the LAN application: {result:?}");
                        }
                        result.unwrap();
                    },
                    async {
                        future::zip(application, simulated_device).await;
                    },
                )
                .await;
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                panic!(
                    "XiaomiRuntime LAN lifecycle exceeded its hard deadline: wire_stage={}, features={}, admission={:?}, publication={}, ready_seen={}, ready_stage={}",
                    wire_stage.get(),
                    runtime.service().features().len(),
                    runtime.status().admission.status,
                    // Publication remains pending while storage is Busy and becomes rejected
                    // only when the authenticated proof is no longer eligible.
                    lifecycle_control.publication.get() as u8,
                    lifecycle_control.ready_seen.get(),
                    lifecycle_control.ready_stage.get(),
                )
            },
        ));
        assert!(application_done.get());
        assert_eq!(
            subscribe_count.get(),
            1,
            "notification cancelled an in-flight SUB"
        );
        assert_eq!(
            unsubscribe_count.get(),
            1,
            "source deselection must send one matching UNSUB"
        );
    }

    fn exercise_catalog_and_subscription_completion(
        reject_subscription: bool,
        lan_timeout: bool,
        refresh_cloud: bool,
        cloud_lifecycle: bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
        let certificate = crate::xiaomi::test_support::sign_csr(
            &identity.csr_pem,
            unix_time() - 60,
            unix_time() + 30 * 24 * 60 * 60,
        )
        .unwrap();
        store
            .xiaomi()
            .replace(&crate::storage::XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri:
                    "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef"
                        .into(),
                tokens: crate::storage::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: unix_time() + 30 * 24 * 60 * 60,
                    refresh_at: unix_time() + 20 * 24 * 60 * 60,
                },
                virtual_did: identity.virtual_did,
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            })
            .unwrap();
        let runtime = XiaomiRuntime::new(store.clone()).unwrap();
        let snapshot = store.xiaomi().snapshot().unwrap();
        let type_urn = "urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1";
        let initial_owned = crate::xiaomi::cloud::OwnedCatalog {
            uid: "10001".into(),
            homes: vec![crate::xiaomi::cloud::OwnedHome {
                id: "home".into(),
                name: "Home".into(),
                group_id: "group".into(),
                dids: vec!["device.did".into()],
                rooms: vec![],
            }],
            devices: vec![crate::xiaomi::cloud::CloudDevice {
                did: "device.did".into(),
                uid: Some("10001".into()),
                name: "Light".into(),
                model: "yeelink.light.ml9".into(),
                spec_type: Some(type_urn.into()),
                pid: None,
                token: None,
                online: Some(true),
                local_ip: None,
                parent_id: None,
            }],
        };
        let specifications = HashMap::from([(
            type_urn.to_owned(),
            include_str!("../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned(),
        )]);
        let catalog = super::super::AdmissionCatalog {
            account: crate::device::AccountId::new("10001").unwrap(),
            session_generation: snapshot.session_generation,
            catalog: assemble_catalog(&initial_owned, &specifications).unwrap(),
            specifications,
        };
        let network = crate::xiaomi::discovery::NetworkSnapshot::select(
            vec![crate::xiaomi::discovery::InterfaceRecord {
                index: 7,
                name: "en-test".into(),
                address: "192.0.2.2".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                up: true,
                point_to_point: false,
                loopback: false,
                link_type: crate::xiaomi::discovery::LinkType::Ethernet,
            }],
            None,
        )
        .unwrap();
        let endpoint = GatewayEndpoint {
            interface_index: 7,
            source_address: "192.0.2.2".parse().unwrap(),
            address: "192.0.2.10".parse().unwrap(),
            port: 8883,
        };
        let proof = AuthenticatedGateway {
            account: catalog.account.clone(),
            session_generation: snapshot.session_generation,
            candidate: GatewayCandidate {
                gateway_did: 123,
                home_group: "group".into(),
                endpoints: vec![endpoint.clone()],
                unverified: false,
            },
            selected_endpoint: endpoint,
            network,
            evidence: crate::xiaomi::gateway::GatewayEvidence {
                gateway_did: 123,
                peer_did: "123".into(),
                epoch: NetworkEpoch::new(7),
                devices: vec![crate::xiaomi::gateway::GatewayDevice {
                    did: "device.did".into(),
                    name: "Light".into(),
                    urn: type_urn.into(),
                    model: "yeelink.light.ml9".into(),
                    online: Some(true),
                    spec_v2_access: Some(true),
                    push_available: Some(true),
                }],
            },
        };
        let (_mqtt, mqtt, messages) = crate::xiaomi::mqtt::MqttConnection::new(
            MqttConfig::new("catalog-test", None, Duration::from_secs(60))
                .with_endpoint("127.0.0.1", 9),
        )
        .unwrap();
        let (_session, handle, notifications) = crate::xiaomi::gateway::GatewaySession::new(
            "catalog-test",
            123,
            "123",
            NetworkEpoch::new(7),
            mqtt,
            messages,
        )
        .unwrap();
        let authority = runtime.session_authority();
        let mut holder = runtime.inner.runner.borrow_mut();
        let runner = holder.as_mut().unwrap();
        runner.admission.invalidate(NetworkEpoch::new(7));
        runner.network_update = Some(NetworkUpdate {
            epoch: NetworkEpoch::new(7),
            snapshot: proof.network.clone(),
        });
        runner
            .admission
            .observe_gateway(proof.clone(), &catalog)
            .unwrap();
        let admitted = runner.admission.snapshot().unwrap();
        runner.catalog = Some(catalog);
        let parts = super::super::GatewayRuntimeParts {
            handle,
            notifications,
            evidence: proof.evidence.clone(),
            task: future::pending().boxed_local(),
        };
        let mut gateway = active_gateway(proof, authority, parts);
        gateway.wire_generation = 1;
        gateway.selected_dids = BTreeSet::from(["device.did".into()]);
        runner.gateways.insert(123, gateway);
        runtime.rebuild_gateway_routes(runner, &admitted);
        assert_eq!(runner.gateways[&123].desired_dids.len(), 1);
        runtime.inner.state.reconcile(&admitted);
        let device = admitted.features[0].identity.physical.clone();
        let token = runtime
            .inner
            .state
            .select_push_source(&device, PushSource::Gateway(123), 123, 1)
            .unwrap();
        assert!(runtime.inner.state.acknowledge(&token, 123, 1));
        runner
            .gateways
            .get_mut(&123)
            .unwrap()
            .tokens
            .insert(device.clone(), token);
        if refresh_cloud {
            runner
                .admission
                .observe_cloud(&CloudEvidence {
                    account: crate::device::AccountId::new("10001").unwrap(),
                    session_generation: snapshot.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            runtime.inner.registry.revoke_cloud(&device);
            runner.cloud_routes.clear();
            let evidence = runner.gateways[&123].proof.evidence.clone();
            runtime
                .finish_gateway_operation(
                    runner,
                    GatewayOperationResult::Refreshed {
                        did: 123,
                        result: Ok(evidence),
                    },
                )
                .unwrap();
            assert!(
                RuntimeTransports::new(runtime.inner.registry.clone())
                    .available_paths(&device)
                    .cloud
            );
            return;
        }
        if lan_timeout {
            runner
                .admission
                .observe_cloud(&CloudEvidence {
                    account: crate::device::AccountId::new("10001").unwrap(),
                    session_generation: snapshot.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            runner.admission.remove_gateway(123).unwrap();
            let cloud_snapshot = runner.admission.snapshot().unwrap();
            runtime.inner.state.reconcile(&cloud_snapshot);
            runtime.rebuild_gateway_routes(runner, &cloud_snapshot);
            let lan_feature = cloud_snapshot.features[0].runtime.clone();
            runtime.inner.commands.register(lan_feature);
            runtime
                .service()
                .set_state_availability(&cloud_snapshot.features[0].identity, true);

            let (session, handle, _notifications, lan_device, token) =
                crate::xiaomi::lan::session_pair("lumi.acpartner.mcn02");
            let mut lan_task = session.run().boxed_local();
            let evidence = block_on(future::race(
                async {
                    future::zip(
                        handle.authenticate(
                            crate::xiaomi::lan::LanProperty { siid: 2, piid: 1 },
                            Instant::now() + Duration::from_secs(1),
                            crate::xiaomi::lan::LanSendGuard::new(),
                        ),
                        crate::xiaomi::lan::reject_native_authentication_probe(&lan_device, &token),
                    )
                    .await
                    .0
                    .unwrap()
                },
                async {
                    let result = lan_task.as_mut().await;
                    panic!("LAN session stopped during authentication: {result:?}")
                },
            ));
            assert!(!evidence.native_supported);
            let lan_authority = runtime.session_authority();
            let target = crate::xiaomi::lan::LanTarget::for_test(
                42,
                "lumi.acpartner.mcn02",
                "127.0.0.1:54321".parse().unwrap(),
                crate::xiaomi::discovery::NetworkInterface {
                    index: 1,
                    name: "test-loopback".into(),
                    address: std::net::Ipv4Addr::LOCALHOST,
                    netmask: "255.0.0.0".parse().unwrap(),
                    prefix_len: 8,
                },
                NetworkEpoch::new(7),
                [0x31; 16],
            );
            let control = LanConnectionControl::new(runtime.inner.wake.clone());
            control.activate();
            let resources = Rc::new(LanAttemptResources {
                device: device.clone(),
                handle,
                authority: lan_authority,
                registry: runtime.inner.registry.clone(),
                state: runtime.inner.state.clone(),
                route: RefCell::new(None),
                token: RefCell::new(None),
                closed: Cell::new(false),
            });
            resources.install_route(Some(cloud_snapshot.features[0].runtime.descriptor.clone()));
            let (_fact_sender, fact_receiver) = flume::bounded(1);
            runner.lans.insert(
                device.clone(),
                ActiveLan {
                    targets: vec![target],
                    control,
                    current: Some((1, resources)),
                    task: async move {
                        let _ = lan_task.await;
                    }
                    .boxed_local(),
                    fact: lan_connection_fact(fact_receiver),
                },
            );
            let (cloud_base, cloud_requests) =
                crate::xiaomi::test_support::mock_server_with_accept_timeout(
                    vec![crate::xiaomi::test_support::MockResponse::json(
                        200,
                        r#"{"code":0,"result":[{"did":"device.did","siid":2,"piid":2,"code":0}]}"#,
                    )],
                    Duration::from_secs(5),
                );
            let cloud_authority = runtime.session_authority();
            runtime.inner.registry.install_cloud(
                device.clone(),
                Rc::new(CloudClient::for_test(&cloud_base, Duration::from_millis(500)).unwrap()),
                "access",
                i64::MAX,
                cloud_authority.clone(),
            );
            let failed = runtime.service().command(
                &cloud_snapshot.features[0].identity,
                crate::device::DeviceCommand::SetPower(true),
            );
            let fallback = runtime.service().command(
                &cloud_snapshot.features[0].identity,
                crate::device::DeviceCommand::SetBrightness(
                    crate::device::Percent::new(25.0).unwrap(),
                ),
            );
            let completed = block_on(future::race(
                async {
                    let task = runner.lans.get_mut(&device).unwrap().task.as_mut();
                    let application = async {
                        future::zip(runtime.inner.commands.run_until_idle(), async {
                            let (request, _) =
                                crate::xiaomi::lan::receive_request_for_test(&lan_device, &token)
                                    .await;
                            assert_eq!(request["method"], "set_power");
                            assert_eq!(request["params"], serde_json::json!(["on"]));
                        })
                        .await;
                        true
                    };
                    future::or(application, async {
                        task.await;
                        panic!("LAN session stopped before the timed-out command")
                    })
                    .await
                },
                async {
                    Timer::after(Duration::from_secs(5)).await;
                    false
                },
            ));
            assert!(
                completed,
                "LAN timeout command did not finish within its hard deadline"
            );
            assert_eq!(block_on(failed), crate::device::CommandOutcome::Ambiguous);
            let fallback_outcome = block_on(fallback);
            let request = cloud_requests.recv_timeout(Duration::from_secs(1));
            let request_received = request.is_ok();
            assert_eq!(
                fallback_outcome,
                crate::device::CommandOutcome::Accepted,
                "queued fallback request_received={request_received}, authority={:?}, can_control={}, diagnostics={:?}",
                cloud_authority.check(),
                runtime
                    .service()
                    .can_control(&cloud_snapshot.features[0].identity),
                runtime.inner.commands.drain_diagnostics(),
            );
            let request = request.expect("queued cloud fallback did not send a loopback request");
            assert!(request.target.ends_with("/miotspec/prop/set"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["params"][0]["value"],
                serde_json::json!(25)
            );
            assert!(
                cloud_requests.try_recv().is_err(),
                "the queued target was sent to Cloud more than once"
            );
            runtime.drain_transport_failures(runner).unwrap();
            assert!(runner.lans[&device].current.is_none());
            assert!(runner.lans[&device].control.reconnect.get() > 1);
            return;
        }
        runtime
            .finish_gateway_operation(
                runner,
                GatewayOperationResult::Selected {
                    did: 123,
                    desired: BTreeSet::from(["device.did".into()]),
                    result: if reject_subscription {
                        Err(crate::xiaomi::gateway::GatewayError::for_test(
                            crate::xiaomi::gateway::GatewayErrorKind::Transport,
                        ))
                    } else {
                        Ok(2)
                    },
                },
            )
            .unwrap();
        if reject_subscription {
            assert!(runner.gateways.contains_key(&123));
            assert_eq!(runner.gateway_routes.get(&device), Some(&123));
            assert!(runner.gateways[&123].tokens.is_empty());
            assert!(
                runtime
                    .status()
                    .diagnostics
                    .iter()
                    .any(|diagnostic| matches!(
                        diagnostic,
                        XiaomiRuntimeDiagnostic::Boundary(XiaomiBoundaryDiagnostic {
                            component: XiaomiRuntimeComponent::Gateway,
                            stage: XiaomiFailureStage::Subscribe,
                            code: XiaomiSafeFailureCode::Unavailable,
                            subject: Some(subject),
                        }) if subject == "123"
                    ))
            );
            return;
        }
        runtime
            .apply_gateway_notification(
                runner,
                123,
                GatewayNotification::Property {
                    did: "device.did".into(),
                    siid: 2,
                    piid: 1,
                    value: Some(crate::xiaomi::catalog::WireValue::Boolean(true)),
                    epoch: 7,
                    generation: 2,
                },
            )
            .unwrap();
        let feature = admitted.features[0].identity.clone();
        assert!(
            runtime
                .service()
                .snapshot(&feature)
                .and_then(|snapshot| snapshot.property(crate::device::Property::Power).cloned())
                .is_some_and(|state| matches!(
                    state,
                    crate::device::PropertyState::Current {
                        value: crate::device::PropertyValue::Power(true),
                        ..
                    }
                )),
            "a retained DID rejected the new subscription generation"
        );

        if cloud_lifecycle {
            runtime.drop_gateway(runner, 123).unwrap();
            runner
                .admission
                .observe_cloud(&CloudEvidence {
                    account: crate::device::AccountId::new("10001").unwrap(),
                    session_generation: snapshot.session_generation,
                    status: CloudStatus::Ready,
                })
                .unwrap();
            runtime.reconcile(&runner.admission).unwrap();
            runtime.sync_cloud_routes(runner).unwrap();
            let desired = BTreeSet::from(["device.did".to_owned()]);
            let authority = runtime.session_authority();
            let CloudRuntimeHarness {
                handle,
                notifications,
                session,
                subscribed,
                commands: broker_commands,
                broker,
            } = real_cloud_runtime_connection("device.did");
            let control = CloudConnectionControl::new(desired.clone());
            let (facts, receiver) = flume::bounded(256);
            runner.cloud_notifications = Some(ActiveCloudNotifications {
                authority: authority.clone(),
                control: control.clone(),
                task: active_cloud_connection_lifetime(
                    handle,
                    authority,
                    notifications,
                    session,
                    BTreeSet::new(),
                    control,
                    facts,
                ),
                fact: cloud_connection_fact(receiver),
                desired: BTreeSet::new(),
                generation: 0,
                session_id: 0,
                tokens: BTreeMap::new(),
                publication: None,
            });
            runner.next_auth = Instant::now() + Duration::from_secs(60);
            runner.next_network = Instant::now() + Duration::from_secs(60);
            runner.next_catalog = Instant::now() + Duration::from_secs(60);
            runner.next_browser_start = Instant::now() + Duration::from_secs(60);
            runner.auth_task = None;
            runner.network_task = None;
            runner.catalog_task = None;
            runner.browser_read = None;
            runtime.inner.refresh_requested.set(false);
            drop(holder);
            let completed = block_on(future::or(
                async {
                    let run = async {
                        runtime.run().await.unwrap();
                    };
                    let application = async {
                        while subscribed.try_recv().is_err() {
                            Timer::after(Duration::from_millis(5)).await;
                        }
                        broker_commands
                            .send(CloudBrokerCommand::Acknowledge)
                            .unwrap();
                        wait_until(Duration::from_secs(1), || {
                            runtime
                                .inner
                                .state
                                .has_healthy_source_for_test(&feature.physical, PushSource::Cloud)
                        })
                        .await;
                        broker_commands
                            .send(CloudBrokerCommand::Property(false))
                            .unwrap();
                        wait_until(Duration::from_secs(1), || {
                            runtime
                                .service()
                                .snapshot(&feature)
                                .and_then(|snapshot| {
                                    snapshot.property(crate::device::Property::Power).cloned()
                                })
                                .is_some_and(|state| {
                                    matches!(
                                        state,
                                        crate::device::PropertyState::Current {
                                            value: crate::device::PropertyValue::Power(false),
                                            ..
                                        }
                                    )
                                })
                        })
                        .await;
                        broker_commands.send(CloudBrokerCommand::Offline).unwrap();
                        wait_until(Duration::from_secs(1), || {
                            !runtime.service().is_available(&feature)
                        })
                        .await;
                        broker_commands.send(CloudBrokerCommand::Online).unwrap();
                        broker_commands
                            .send(CloudBrokerCommand::Property(true))
                            .unwrap();
                        wait_until(Duration::from_secs(1), || {
                            runtime.service().is_available(&feature)
                                && runtime
                                    .service()
                                    .snapshot(&feature)
                                    .and_then(|snapshot| {
                                        snapshot.property(crate::device::Property::Power).cloned()
                                    })
                                    .is_some_and(|state| {
                                        matches!(
                                            state,
                                            crate::device::PropertyState::Current {
                                                value: crate::device::PropertyValue::Power(true),
                                                ..
                                            }
                                        )
                                    })
                        })
                        .await;
                        runtime.stop();
                    };
                    future::zip(run, application).await;
                    true
                },
                async {
                    Timer::after(Duration::from_secs(3)).await;
                    false
                },
            ));
            let _ = broker_commands.send(CloudBrokerCommand::Stop);
            broker.join().unwrap();
            assert!(
                completed,
                "Cloud coordinator loopback exceeded its hard deadline"
            );
            return;
        }

        let replacement = crate::xiaomi::cloud::OwnedCatalog {
            uid: "10001".into(),
            homes: vec![crate::xiaomi::cloud::OwnedHome {
                id: "home".into(),
                name: "Home".into(),
                group_id: "group".into(),
                dids: vec!["unresolved.did".into()],
                rooms: vec![],
            }],
            devices: vec![crate::xiaomi::cloud::CloudDevice {
                did: "unresolved.did".into(),
                uid: Some("10001".into()),
                name: "Unresolved".into(),
                model: "vendor.unknown.x".into(),
                spec_type: Some("urn:miot-spec-v2:device:unknown:0000FFFF:vendor-x:1".into()),
                pid: None,
                token: None,
                online: Some(true),
                local_ip: None,
                parent_id: None,
            }],
        };
        runtime
            .finish_catalog_refresh(
                runner,
                CatalogTaskResult::Owned {
                    snapshot,
                    owned: Some(replacement),
                },
            )
            .unwrap();
        assert!(
            runner.catalog_task.is_some(),
            "spec resolution was not left pending"
        );
        if let Some(gateway) = runner.gateways.get(&123) {
            assert!(gateway.desired_dids.is_empty());
            assert!(gateway.control.desired.borrow().is_empty());
        }
        assert!(runtime.service().features().is_empty());
    }

    #[test]
    fn outer_runtime_admits_controls_and_applies_a_real_gateway_push() {
        let (cloud_base, cloud_requests) = crate::xiaomi::test_support::dynamic_mock_server(
            8,
            |request| {
                if request.target.ends_with("/homeroom/gethome") {
                    return crate::xiaomi::test_support::MockResponse::json(401, r#"{"code":401}"#);
                }
                if request.target.contains("/oauth/get_token") {
                    return crate::xiaomi::test_support::MockResponse::json(401, r#"{"code":401}"#);
                }
                let body: serde_json::Value = serde_json::from_str(&request.body)
                    .unwrap_or_else(|error| panic!("unexpected {} body: {error}", request.target));
                let result = body["params"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| {
                        let piid = item["piid"].as_u64().unwrap();
                        if request.target.ends_with("/get") {
                            serde_json::json!({
                                "did":"device.did","siid":2,"piid":piid,"code":0,
                                "value": match piid { 1 => serde_json::json!(false), 2 => serde_json::json!(50), 3 => serde_json::json!(4000), _ => serde_json::json!(0) }
                            })
                        } else {
                            serde_json::json!({"did":"device.did","siid":2,"piid":piid,"code":0})
                        }
                    })
                    .collect::<Vec<_>>();
                crate::xiaomi::test_support::MockResponse::json(
                    200,
                    &serde_json::json!({"code":0,"result":result}).to_string(),
                )
            },
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let broker = thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let mut buffer = BytesMut::new();
            assert!(matches!(
                mqtt_packet(&mut stream, &mut buffer),
                v5::Packet::Connect(_)
            ));
            mqtt_write(
                &mut stream,
                &v5::Packet::ConnAck(v5::ConnAck::new(v5::ConnectReturnCode::Success, false)),
            );
            let v5::Packet::Subscribe(initial) = mqtt_packet(&mut stream, &mut buffer) else {
                panic!("expected initial subscriptions")
            };
            mqtt_write(
                &mut stream,
                &v5::Packet::SubAck(v5::SubAck::new(
                    initial.pkid,
                    vec![v5::SubscribeReasonCode::QoS2; initial.filters.len()],
                )),
            );
            let devices = receive_request(&mut stream, &mut buffer);
            reply(
                &mut stream,
                &mut buffer,
                41,
                devices.return_topic.as_deref().unwrap(),
                devices.mid,
                r#"{"devList":{"device.did":{"name":"Light","urn":"urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1","model":"yeelink.light.ml9","online":true,"specV2Access":true,"pushAvailable":true}}}"#,
            );
            let mut pushed = false;
            let mut set_count = 0;
            while set_count == 0 {
                match mqtt_packet(&mut stream, &mut buffer) {
                    v5::Packet::PingReq => {
                        stream.write_all(&[0xd0, 0x00]).unwrap();
                    }
                    v5::Packet::Subscribe(subscribe) => {
                        mqtt_write(
                            &mut stream,
                            &v5::Packet::SubAck(v5::SubAck::new(
                                subscribe.pkid,
                                vec![v5::SubscribeReasonCode::QoS2; subscribe.filters.len()],
                            )),
                        );
                        if !pushed {
                            let payload = crate::xiaomi::gateway::MipsEnvelope {
                                mid: 0,
                                return_topic: None,
                                payload: r#"{"did":"device.did","siid":2,"piid":1,"value":false}"#
                                    .into(),
                                from: Some("local".into()),
                            }
                            .encode()
                            .unwrap();
                            let mut publish = v5::Publish::new(
                                "virtual/appMsg/notify/iot/device.did/property/2.1",
                                QoS::AtMostOnce,
                                payload,
                            );
                            publish.pkid = 0;
                            mqtt_write(&mut stream, &v5::Packet::Publish(publish));
                            pushed = true;
                        }
                    }
                    v5::Packet::Publish(publish) => {
                        let request =
                            crate::xiaomi::gateway::MipsEnvelope::decode(&publish.payload).unwrap();
                        mqtt_write(
                            &mut stream,
                            &v5::Packet::PubRec(v5::PubRec::new(publish.pkid)),
                        );
                        loop {
                            match mqtt_packet_ignoring_ping(&mut stream, &mut buffer) {
                                v5::Packet::PubRel(_) => break,
                                v5::Packet::Subscribe(subscribe) => {
                                    mqtt_write(
                                        &mut stream,
                                        &v5::Packet::SubAck(v5::SubAck::new(
                                            subscribe.pkid,
                                            vec![
                                                v5::SubscribeReasonCode::QoS2;
                                                subscribe.filters.len()
                                            ],
                                        )),
                                    );
                                    if !pushed {
                                        let payload = crate::xiaomi::gateway::MipsEnvelope {
                                            mid: 0,
                                            return_topic: None,
                                            payload: r#"{"did":"device.did","siid":2,"piid":1,"value":false}"#
                                                .into(),
                                            from: Some("local".into()),
                                        }
                                        .encode()
                                        .unwrap();
                                        let mut notification = v5::Publish::new(
                                            "virtual/appMsg/notify/iot/device.did/property/2.1",
                                            QoS::AtMostOnce,
                                            payload,
                                        );
                                        notification.pkid = 0;
                                        mqtt_write(&mut stream, &v5::Packet::Publish(notification));
                                        pushed = true;
                                    }
                                }
                                packet => panic!("expected PUBREL, got {packet:?}"),
                            }
                        }
                        mqtt_write(
                            &mut stream,
                            &v5::Packet::PubComp(v5::PubComp::new(publish.pkid)),
                        );
                        let value: serde_json::Value =
                            serde_json::from_str(&request.payload).unwrap();
                        let rpc = &value["rpc"];
                        if rpc.is_null() {
                            let piid = value["piid"].as_u64().unwrap();
                            let result = serde_json::json!({
                                "value": match piid { 1 => serde_json::json!(false), 2 => serde_json::json!(50), 3 => serde_json::json!(4000), _ => serde_json::json!(0) }
                            });
                            reply(
                                &mut stream,
                                &mut buffer,
                                50 + piid as u16,
                                request.return_topic.as_deref().unwrap(),
                                request.mid,
                                &result.to_string(),
                            );
                        } else {
                            assert_eq!(rpc["method"], "set_properties");
                            assert_eq!(rpc["params"][0]["did"], "device.did");
                            assert_eq!(rpc["params"][0]["siid"], 2);
                            assert_eq!(rpc["params"][0]["piid"], 1);
                            assert_eq!(rpc["params"][0]["value"], false);
                            set_count += 1;
                            reply(
                                &mut stream,
                                &mut buffer,
                                80,
                                request.return_topic.as_deref().unwrap(),
                                request.mid,
                                r#"{"result":[{"did":"device.did","siid":2,"piid":1,"code":0}]}"#,
                            );
                        }
                    }
                    packet => panic!("unexpected broker packet: {packet:?}"),
                }
            }
            assert!(pushed);
            assert_eq!(set_count, 1);
            thread::sleep(Duration::from_millis(50));
        });

        block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = crate::xiaomi::certificate::ClientIdentity::generate("10001").unwrap();
            let certificate = crate::xiaomi::test_support::sign_csr(
                &identity.csr_pem,
                unix_time() - 60,
                unix_time() + 30 * 24 * 60 * 60,
            )
            .unwrap();
            store.xiaomi().replace(&crate::storage::XiaomiRecord {
                uid: "10001".into(),
                region: "cn".into(),
                oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
                redirect_uri: "http://homeassistant.local:8123/api/webhook/0123456789abcdef0123456789abcdef".into(),
                tokens: crate::storage::TokenSet {
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    expires_at: unix_time() + 30 * 24 * 60 * 60,
                    refresh_at: unix_time() + 20 * 24 * 60 * 60,
                },
                virtual_did: identity.virtual_did.clone(),
                private_key_pem: identity.private_key_pem,
                certificate_pem: certificate,
            }).unwrap();
            let runtime = XiaomiRuntime::new(store.clone()).unwrap();
            let spec = include_str!("../../../tests/fixtures/miot_specs/yeelink.light.ml9.json");
            let type_urn = "urn:miot-spec-v2:device:light:0000A001:yeelink-ml9:1";
            let owned = crate::xiaomi::cloud::OwnedCatalog {
                uid: "10001".into(),
                homes: vec![crate::xiaomi::cloud::OwnedHome {
                    id: "home".into(),
                    name: "Home".into(),
                    group_id: "group".into(),
                    dids: vec!["device.did".into()],
                    rooms: vec![],
                }],
                devices: vec![crate::xiaomi::cloud::CloudDevice {
                    did: "device.did".into(),
                    uid: Some("10001".into()),
                    name: "Light".into(),
                    model: "yeelink.light.ml9".into(),
                    spec_type: Some(type_urn.into()),
                    pid: None,
                    token: None,
                    online: Some(true),
                    local_ip: None,
                    parent_id: None,
                }],
            };
            let specifications = HashMap::from([(type_urn.to_owned(), spec.to_owned())]);
            let catalog =
                crate::xiaomi::catalog::assemble_catalog(&owned, &specifications).unwrap();
            let snapshot = store.xiaomi().snapshot().unwrap();
            let network = NetworkUpdate {
                epoch: NetworkEpoch::new(9),
                snapshot: crate::xiaomi::discovery::NetworkSnapshot::select(
                    vec![crate::xiaomi::discovery::InterfaceRecord {
                        index: 7,
                        name: "en-test".into(),
                        address: "192.0.2.2".parse().unwrap(),
                        netmask: "255.255.255.0".parse().unwrap(),
                        up: true,
                        point_to_point: false,
                        loopback: false,
                        link_type: crate::xiaomi::discovery::LinkType::Ethernet,
                    }],
                    None,
                )
                .unwrap(),
            };
            let candidate = GatewayCandidate {
                gateway_did: 123,
                home_group: "group".into(),
                endpoints: vec![GatewayEndpoint {
                    interface_index: 7,
                    source_address: "192.0.2.2".parse().unwrap(),
                    address: "192.0.2.10".parse().unwrap(),
                    port: 8883,
                }],
                unverified: false,
            };
            let authority = SessionAuthority::new();
            {
                let mut holder = runtime.inner.runner.borrow_mut();
                let runner = holder.as_mut().unwrap();
                runner.catalog = Some(crate::xiaomi::runtime::AdmissionCatalog {
                    account: crate::device::AccountId::new("10001").unwrap(),
                    session_generation: snapshot.session_generation,
                    catalog,
                    specifications,
                });
                runner.cloud_validated = true;
                runner.cloud =
                    Rc::new(CloudClient::for_test(&cloud_base, Duration::from_secs(1)).unwrap());
                runner.auth = Rc::new(AuthService::new(
                    store.xiaomi(),
                    CloudClient::for_test(&cloud_base, Duration::from_secs(1)).unwrap(),
                ));
                runner.admission.invalidate(NetworkEpoch::new(9));
                runner.network_update = Some(network.clone());
                runner.network = None;
                runner.next_network = Instant::now() + Duration::from_secs(60);
                runner.next_auth = Instant::now() + Duration::from_secs(60);
                runner.next_catalog = Instant::now() + Duration::from_secs(60);
                runtime.inner.refresh_requested.set(false);
                let startup_authority = authority.clone();
                let startup = async move {
                    crate::xiaomi::runtime::start_gateway_plain_for_test(
                        match address {
                            std::net::SocketAddr::V4(value) => value,
                            _ => unreachable!(),
                        },
                        MqttConfig::new("runtime", None, Duration::from_secs(5)),
                        &identity.virtual_did,
                        123,
                        "192.0.2.10",
                        NetworkEpoch::new(9),
                        startup_authority.mqtt_guard(),
                        Instant::now() + Duration::from_secs(3),
                    )
                    .await
                }
                .boxed_local();
                let endpoint = candidate.endpoints[0].clone();
                let interface = network
                    .snapshot
                    .interfaces_with_index(endpoint.interface_index)
                    .next()
                    .unwrap()
                    .clone();
                let control = GatewayConnectionControl::new();
                let (fact_sender, fact_receiver) = flume::bounded(256);
                let config = GatewayConnectionConfig {
                    candidate: candidate.clone(),
                    endpoints: vec![(endpoint, interface)],
                    network: network.clone(),
                    virtual_did: "test-virtual-did".into(),
                    private_key_pem: String::new(),
                    certificate_pem: String::new(),
                    authority: authority.clone(),
                    setup: runner.gateway_setup.clone(),
                    startup: RefCell::new(Some(startup)),
                };
                runner.connecting_gateways.insert(
                    123,
                    ConnectingGateway {
                        candidate,
                        network,
                        account: crate::device::AccountId::new("10001").unwrap(),
                        session_generation: snapshot.session_generation,
                        authority,
                        task: gateway_full_connection_lifetime(
                            config,
                            control.clone(),
                            fact_sender,
                        ),
                        fact: gateway_connection_fact(fact_receiver),
                        control,
                        attempt: None,
                    },
                );
            }
            let service = runtime.service();
            let credential_store = store.xiaomi();
            let app_completed = Rc::new(Cell::new(false));
            let completed_by_app = app_completed.clone();
            let app = async {
                let feature =
                    loop {
                        if let Some(feature) = service.features().into_iter().find(|feature| {
                            feature.identity.role == crate::device::FeatureRole::Light
                        }) && service.is_available(&feature.identity)
                        {
                            break feature.identity;
                        }
                        Timer::after(Duration::from_millis(10)).await;
                    };
                loop {
                    if service
                        .snapshot(&feature)
                        .and_then(|snapshot| {
                            snapshot.property(crate::device::Property::Power).cloned()
                        })
                        .is_some_and(|state| {
                            matches!(
                                state,
                                crate::device::PropertyState::Current {
                                    value: crate::device::PropertyValue::Power(false),
                                    ..
                                }
                            )
                        })
                    {
                        break;
                    }
                    Timer::after(Duration::from_millis(10)).await;
                }
                let ticket =
                    service.command(&feature, crate::device::DeviceCommand::SetPower(false));
                assert_eq!(
                    ticket.await,
                    crate::device::CommandOutcome::Accepted,
                    "token recovery status: {:?}",
                    runtime.status()
                );
                runtime.refresh();
                loop {
                    if matches!(
                        runtime.auth_report().authentication,
                        crate::xiaomi::auth::AuthenticationState::SignInRequired(_)
                    ) {
                        break;
                    }
                    Timer::after(Duration::from_millis(10)).await;
                }
                credential_store
                    .update_tokens(&crate::storage::TokenSet {
                        access_token: "new-access".into(),
                        refresh_token: "new-refresh".into(),
                        expires_at: unix_time() + 30 * 24 * 60 * 60,
                        refresh_at: unix_time() + 20 * 24 * 60 * 60,
                    })
                    .unwrap();
                Timer::after(Duration::from_millis(250)).await;
                let ticket =
                    service.command(&feature, crate::device::DeviceCommand::SetPower(true));
                assert_eq!(
                    ticket.await,
                    crate::device::CommandOutcome::Accepted,
                    "token recovery status: {:?}",
                    runtime.status()
                );
                assert!(
                    service
                        .snapshot(&feature)
                        .and_then(|snapshot| snapshot
                            .property(crate::device::Property::Power)
                            .cloned())
                        .is_some_and(|state| matches!(
                            state,
                            crate::device::PropertyState::Current {
                                value: crate::device::PropertyValue::Power(false),
                                ..
                            }
                        ))
                );
                completed_by_app.set(true);
                runtime.stop();
            };
            let completed = future::or(
                async {
                    future::or(
                        async {
                            runtime.run().await.unwrap();
                        },
                        app,
                    )
                    .await;
                    true
                },
                async {
                    Timer::after(Duration::from_secs(5)).await;
                    false
                },
            )
            .await;
            assert!(
                completed,
                "outer Xiaomi runtime scenario timed out: {:?}",
                runtime.status()
            );
            assert!(
                app_completed.get(),
                "runtime exited before application assertions completed"
            );
        });
        broker.join().unwrap();
        let request = loop {
            let request = cloud_requests.recv_timeout(Duration::from_secs(1)).unwrap();
            if request.target.ends_with("/set") {
                break request;
            }
        };
        assert_eq!(request.target, "/app/v2/miotspec/prop/set");
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearernew-access")
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&request.body).unwrap()["params"][0]["value"],
            true
        );
        assert!(
            cloud_requests
                .try_iter()
                .all(|request| !request.target.ends_with("/set"))
        );
    }
}
