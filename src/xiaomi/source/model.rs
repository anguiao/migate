use crate::{
    device::DeviceRegistry,
    storage::StorageError,
    xiaomi::{
        auth::{AuthError, AuthReport},
        catalog::CatalogError,
        central::CentralError,
        cloud::CloudError,
        discovery::DiscoveryError,
    },
};
use event_listener::Event;
use std::{cell::RefCell, error::Error, fmt, rc::Rc};
#[derive(Clone, Debug)]
pub enum SourceError {
    Storage(StorageError),
    Cloud(CloudError),
    Discovery(DiscoveryError),
    Central(CentralError),
    Unavailable(String),
    NoLocalGateway,
    Cancelled,
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => e.fmt(f),
            Self::Cloud(e) => e.fmt(f),
            Self::Discovery(e) => e.fmt(f),
            Self::Central(e) => e.fmt(f),
            Self::Unavailable(e) => f.write_str(e),
            Self::NoLocalGateway => f.write_str("no current local gateway for the selected home"),
            Self::Cancelled => f.write_str("Xiaomi source stopped"),
        }
    }
}
impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(e) => Some(e),
            Self::Cloud(e) => Some(e),
            Self::Discovery(e) => Some(e),
            Self::Central(e) => Some(e),
            _ => None,
        }
    }
}
impl From<StorageError> for SourceError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<CentralError> for SourceError {
    fn from(e: CentralError) -> Self {
        Self::Central(e)
    }
}
impl From<DiscoveryError> for SourceError {
    fn from(e: DiscoveryError) -> Self {
        Self::Discovery(e)
    }
}
impl From<AuthError> for SourceError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Storage(e) => Self::Storage(e),
            AuthError::Cloud(e) => Self::Cloud(e),
        }
    }
}
impl From<CatalogError> for SourceError {
    fn from(e: CatalogError) -> Self {
        match e {
            CatalogError::Cloud(e) => Self::Cloud(e),
            e => Self::Unavailable(e.to_string()),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayStatus {
    Discovering,
    Connecting(String),
    Connected(String),
    Unavailable(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingDevice {
    pub did: String,
    pub name: String,
    pub reason: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceStatus {
    pub authentication: AuthReport,
    pub gateway: GatewayStatus,
    pub catalog_problem: Option<String>,
    pub pending: Vec<PendingDevice>,
}
#[derive(Default)]
pub(super) struct Ready {
    result: RefCell<Option<Result<DeviceRegistry, SourceError>>>,
    completed: std::cell::Cell<bool>,
    changed: Event,
}
impl Ready {
    pub(super) fn finish(&self, result: Result<DeviceRegistry, SourceError>) {
        if !self.completed.replace(true) {
            *self.result.borrow_mut() = Some(result);
            self.changed.notify(usize::MAX);
        }
    }
}
/// Poll the source alongside this one-shot topology wait; the registry remains shared.
pub struct TopologyReady(pub(super) Rc<Ready>);
impl TopologyReady {
    pub async fn wait(self) -> Result<DeviceRegistry, SourceError> {
        loop {
            let listener = self.0.changed.listen();
            if let Some(result) = self.0.result.borrow_mut().take() {
                return result;
            }
            listener.await;
        }
    }
}
