use crate::{
    device::Device,
    storage::{CachedDevice, StorageError},
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
    StalePreparation,
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
            Self::NoLocalGateway => f.write_str("no current local gateway for the device home"),
            Self::StalePreparation => f.write_str("gateway preparation was superseded"),
            Self::Cancelled => f.write_str("Xiaomi device source stopped"),
        }
    }
}
impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
            Self::Discovery(error) => Some(error),
            Self::Central(error) => Some(error),
            _ => None,
        }
    }
}
impl From<StorageError> for SourceError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl From<CentralError> for SourceError {
    fn from(error: CentralError) -> Self {
        Self::Central(error)
    }
}
impl From<DiscoveryError> for SourceError {
    fn from(error: DiscoveryError) -> Self {
        Self::Discovery(error)
    }
}
impl From<AuthError> for SourceError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Storage(e) => Self::Storage(e),
            AuthError::Cloud(e) => Self::Cloud(e),
        }
    }
}
impl From<CatalogError> for SourceError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Cloud(e) => Self::Cloud(e),
            error => Self::Unavailable(error.to_string()),
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
pub struct SourceStatus {
    pub authentication: AuthReport,
    pub gateway: GatewayStatus,
    pub catalog_problem: Option<String>,
}
#[derive(Clone)]
pub struct PreparedDevice {
    pub device: Device,
    pub metadata: CachedDevice,
}
#[derive(Default)]
pub(super) struct Ready {
    result: RefCell<Option<Result<PreparedDevice, SourceError>>>,
    completed: std::cell::Cell<bool>,
    changed: Event,
}
impl Ready {
    pub(super) fn completed(&self) -> bool {
        self.completed.get()
    }
    pub(super) fn finish(&self, result: Result<PreparedDevice, SourceError>) {
        if !self.completed.replace(true) {
            *self.result.borrow_mut() = Some(result);
            self.changed.notify(usize::MAX);
        }
    }
}
/// Poll the source runner alongside this one-shot topology wait. The runner
/// continues owning the live session after the topology has been delivered.
pub struct TopologyReady(pub(super) Rc<Ready>);
impl TopologyReady {
    pub async fn wait(self) -> Result<PreparedDevice, SourceError> {
        loop {
            let listener = self.0.changed.listen();
            if let Some(result) = self.0.result.borrow_mut().take() {
                return result;
            }
            listener.await;
        }
    }
}
