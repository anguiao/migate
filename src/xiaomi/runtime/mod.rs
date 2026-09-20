mod admission;
mod auth;
mod catalog_refresh;
mod cloud_connection;
mod command;
mod connection;
mod discovery;
mod gateway_connection;
mod lan_connection;
mod lifecycle;
mod sessions;
mod startup;
mod state;
mod status;
mod transports;

pub use admission::{
    AdmissionCatalog, AdmissionController, AdmissionFeature, AdmissionSnapshot, AdmissionStatus,
    AuthenticatedGateway, AuthenticatedLan, CloudEvidence, CloudStatus, GatewayPathEvidence,
    LegacyOperationEvidence,
};
pub use command::{
    CommandCompletion, CommandCompletionSubscription, CommandFailureStage, CommandLimits,
    CommandRuntime, CommandTransport, ControlPath, DeviceExecutionGate, DeviceExecutionLease,
    OperationPaths, RuntimeDiagnostic, RuntimeFeature, SendAuthorization, SendGuard,
    SharedSendState, TransportCommand, TransportFailure,
};
pub use startup::{
    CloudNotificationStartup, GatewayStartup, LanOperationEvidence, RunningCloudNotifications,
    RunningGateway, RunningLan, TransportStartupError, start_cloud_notifications, start_gateway,
    start_lan,
};
pub use state::{
    CloudReachabilityUpdate, PushSource, ReadTarget, StateDiagnostic, StateLimits,
    StateReadFailure, StateReadGuard, StateReadRequest, StateReadResult, StateReadTransport,
    StateRuntime, SubscriptionToken,
};
pub use status::{
    XiaomiBoundaryDiagnostic, XiaomiCandidateState, XiaomiCandidateStatus, XiaomiFailureStage,
    XiaomiGatewayStatus, XiaomiRuntimeComponent, XiaomiRuntimeDiagnostic, XiaomiRuntimeStatus,
    XiaomiSafeFailureCode,
};
pub use transports::{CurrentSessionRegistry, RuntimeTransports, SessionAuthority};

#[cfg(test)]
pub(crate) use startup::GatewayRuntimeParts;
#[cfg(test)]
pub(crate) use startup::{start_gateway_plain_for_test, start_lan_session_for_test};
pub(crate) use transports::{RouteFailure, RouteLease};

use crate::{
    device::DeviceService,
    storage::{SessionCheckFailure, StorageError, Store, XiaomiStore},
    xiaomi::{
        auth::{AuthError, AuthReport, AuthService},
        cloud::{CloudClient, CloudError},
        discovery::NetworkEpoch,
    },
};
use auth::AuthMaintenance;
use catalog_refresh::CatalogRefresh;
use discovery::NetworkDiscovery;
use event_listener::Event;
use sessions::DeviceSessions;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, VecDeque},
    fmt,
    rc::Rc,
    time::Duration,
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

struct Runner {
    auth: AuthMaintenance,
    catalog_refresh: CatalogRefresh,
    discovery: NetworkDiscovery,
    sessions: DeviceSessions,
}

impl XiaomiRuntime {
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
                    auth: AuthMaintenance::new(Rc::new(auth), xiaomi.clone(), snapshot),
                    catalog_refresh: CatalogRefresh::new(cloud, runtime_devices.clone()),
                    discovery: NetworkDiscovery::new(),
                    sessions: DeviceSessions::new(admission),
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

    pub fn check_failure(&self) -> Result<(), StorageError> {
        if let Some(error) = self.inner.failure.borrow().clone() {
            return Err(error);
        }
        Ok(())
    }
}

impl Inner {
    fn runner_path(&self) -> &std::path::Path {
        self.xiaomi.path()
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

fn unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_secs().try_into().unwrap_or(i64::MAX)
        })
}
