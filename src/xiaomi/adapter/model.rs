use crate::{
    device::CommandError,
    storage::StorageError,
    xiaomi::{
        auth::{AuthError, AuthReport},
        capability::PropertyBinding,
        catalog::CatalogError,
        central::CentralError,
        cloud::CloudError,
        discovery::DiscoveryError,
    },
};
use event_listener::Event;
use std::{cell::RefCell, error::Error, fmt, rc::Rc};

#[derive(Clone, Debug)]
pub enum AdapterError {
    Storage(StorageError),
    Cloud(CloudError),
    Discovery(DiscoveryError),
    Central(CentralError),
    Unavailable(String),
    NoLocalGateway,
    Cancelled,
}
impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(e) => e.fmt(f),
            Self::Cloud(e) => e.fmt(f),
            Self::Discovery(e) => e.fmt(f),
            Self::Central(e) => e.fmt(f),
            Self::Unavailable(e) => f.write_str(e),
            Self::NoLocalGateway => f.write_str("no current local gateway for the selected home"),
            Self::Cancelled => f.write_str("Xiaomi adapter stopped"),
        }
    }
}
impl Error for AdapterError {
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
impl From<StorageError> for AdapterError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<CentralError> for AdapterError {
    fn from(e: CentralError) -> Self {
        Self::Central(e)
    }
}
impl From<DiscoveryError> for AdapterError {
    fn from(e: DiscoveryError) -> Self {
        Self::Discovery(e)
    }
}
impl From<AuthError> for AdapterError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Storage(e) => Self::Storage(e),
            AuthError::Cloud(e) => Self::Cloud(e),
        }
    }
}
impl From<CatalogError> for AdapterError {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathKind {
    Local,
    Cloud,
}
impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
        })
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RecoveryStatus {
    #[default]
    None,
    Scheduled(i64),
    Running,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingStatus {
    pub priority: super::routing::PathPriority,
    pub local_connected: bool,
    pub business_degraded: bool,
    pub recovery: RecoveryStatus,
    pub last_switch_reason: Option<String>,
}
impl Default for RoutingStatus {
    fn default() -> Self {
        Self {
            priority: super::routing::PathPriority::LocalFirst,
            local_connected: false,
            business_degraded: false,
            recovery: RecoveryStatus::None,
            last_switch_reason: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum CommandTarget {
    Power(bool),
    Light(crate::device::LightTarget),
}
impl fmt::Display for CommandTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Power(value) => f.write_str(if *value { "on" } else { "off" }),
            Self::Light(target) => {
                use crate::device::LightColorTarget;
                let mut fields = Vec::new();
                if let Some(value) = target.power() {
                    fields.push(format!("power={}", if value { "on" } else { "off" }));
                }
                if let Some(value) = target.brightness_percent() {
                    fields.push(format!("brightness={value}%"));
                }
                if let Some(value) = target.color_target() {
                    fields.push(match value {
                        LightColorTarget::Temperature(value) => format!("temperature={value} K"),
                        LightColorTarget::Rgb(value) => format!(
                            "RGB=#{:02X}{:02X}{:02X}",
                            value.red, value.green, value.blue
                        ),
                    });
                }
                f.write_str(&fields.join(", "))
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct CommandStatus {
    pub id: u64,
    pub binding: PropertyBinding,
    pub target: CommandTarget,
    pub properties: Vec<(PropertyBinding, serde_json::Value)>,
    pub actual_path: Option<PathKind>,
    pub result: Option<Result<(), CommandError>>,
}
impl CommandStatus {
    pub(crate) fn properties_text(&self) -> String {
        if self.properties.is_empty() {
            return "none".into();
        }
        if matches!(self.target, CommandTarget::Power(_)) {
            let binding = self.properties[0].0;
            return format!("{}.{}", binding.siid, binding.piid);
        }
        self.properties
            .iter()
            .map(|(binding, value)| format!("{}.{}={value}", binding.siid, binding.piid))
            .collect::<Vec<_>>()
            .join(", ")
    }
    pub(crate) fn result_text(&self) -> String {
        match &self.result {
            None => "awaiting result".into(),
            Some(Ok(())) if self.properties.is_empty() => "unchanged (no write)".into(),
            Some(Ok(())) => "accepted".into(),
            Some(Err(error)) if matches!(self.target, CommandTarget::Power(_)) => error.to_string(),
            Some(Err(CommandError::Superseded)) => "superseded".into(),
            Some(Err(CommandError::Stopped)) => "stopped".into(),
            Some(Err(CommandError::Cancelled)) => "cancelled".into(),
            Some(Err(error)) => format!("failed: {error}"),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelCommands {
    pub light_actions: Option<crate::device::LightActionDiagnostics>,
    pub channel: String,
    pub inflight: Option<CommandStatus>,
    pub last: Option<CommandStatus>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceStatus {
    pub did: String,
    pub read_only: bool,
    pub usable_control_paths: Vec<PathKind>,
    pub selected_control: Option<PathKind>,
    pub usable_read_paths: Vec<PathKind>,
    pub selected_read: Option<PathKind>,
    /// `None` means the source provides no reliable physical on/off signal.
    pub physical_online: Option<bool>,
    pub notification_source: Option<PathKind>,
    pub unavailable_reason: Option<String>,
    pub sync_limited_reason: Option<String>,
    pub last_switch_reason: Option<String>,
    pub commands: Vec<ChannelCommands>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AdapterStatus {
    pub authentication: AuthReport,
    pub gateway: GatewayStatus,
    pub catalog_problem: Option<String>,
    pub pending: Vec<PendingDevice>,
    pub routing: RoutingStatus,
    pub devices: Vec<DeviceStatus>,
}
#[derive(Default)]
pub(super) struct Ready {
    result: RefCell<Option<Result<(), AdapterError>>>,
    changed: Event,
}
impl Ready {
    pub(super) fn finish(&self, result: Result<(), AdapterError>) {
        let mut current = self.result.borrow_mut();
        if current.is_none() {
            *current = Some(result);
            self.changed.notify(usize::MAX);
        }
    }
}
/// Poll the adapter alongside this one-shot topology wait.
pub struct TopologyReady(pub(super) Rc<Ready>);
impl TopologyReady {
    pub async fn wait(self) -> Result<(), AdapterError> {
        loop {
            let listener = self.0.changed.listen();
            if let Some(result) = self.0.result.borrow().clone() {
                return result;
            }
            listener.await;
        }
    }
}
