use crate::{
    device::CommandError,
    storage::StorageError,
    xiaomi::{
        auth::{AuthError, AuthReport},
        capability::{EventBinding, PropertyBinding},
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
            Self::NoLocalGateway => f.write_str("no current gateway for the selected home"),
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
            AuthError::Account(e) => Self::Unavailable(e.to_string()),
            AuthError::Failure(e) => Self::Unavailable(e.to_string()),
            AuthError::LoginStep { stage, source } => {
                Self::Unavailable(format!("{stage}: {source}"))
            }
            AuthError::Missing(message) => Self::Unavailable(message.into()),
        }
    }
}
impl From<CatalogError> for AdapterError {
    fn from(e: CatalogError) -> Self {
        match e {
            CatalogError::Cloud(e) | CatalogError::Description(e) => Self::Cloud(e),
            e => Self::Unavailable(e.to_string()),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GatewayStatus {
    Discovering,
    Connecting(String),
    Connected(String),
    Unavailable(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingDevice {
    pub(crate) did: String,
    pub(crate) name: String,
    pub(crate) reason: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathKind {
    Local,
    Direct,
    Cloud,
}
impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "hub",
            Self::Direct => "direct",
            Self::Cloud => "cloud",
        })
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RecoveryStatus {
    #[default]
    None,
    Scheduled(i64),
    Running,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RoutingStatus {
    pub(crate) priority: super::routing::PathPriority,
    pub(crate) local_connected: bool,
    pub(crate) last_switch_reason: Option<String>,
}
impl Default for RoutingStatus {
    fn default() -> Self {
        Self {
            priority: super::routing::PathPriority::LocalFirst,
            local_connected: false,
            last_switch_reason: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommandTarget {
    Power(bool),
    Light(crate::device::LightTarget),
    Curtain(crate::device::CurtainCommand),
    Fan(crate::device::FanCommand),
    AirConditioner(crate::device::AirConditionerCommand),
    Heater(crate::device::HeaterCommand),
}
impl fmt::Display for CommandTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Power(value) => f.write_str(if *value { "on" } else { "off" }),
            Self::Curtain(crate::device::CurtainCommand::Target(value)) => write!(f, "{value}%"),
            Self::Curtain(crate::device::CurtainCommand::Stop) => f.write_str("stop"),
            Self::Fan(crate::device::FanCommand::Power(value)) => {
                write!(f, "power {}", if *value { "on" } else { "off" })
            }
            Self::Fan(crate::device::FanCommand::Speed(value)) => write!(f, "speed level {value}"),
            Self::Fan(crate::device::FanCommand::Swing(value)) => {
                write!(f, "swing {}", if *value { "on" } else { "off" })
            }
            Self::AirConditioner(command) => write!(f, "{command:?}"),
            Self::Heater(command) => write!(f, "{command:?}"),
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
/// A planned property write and its path once submitted.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandProperty {
    pub(crate) binding: PropertyBinding,
    pub(crate) value: serde_json::Value,
    pub(crate) path: Option<PathKind>,
    /// A real native operation description for an action/legacy step.
    pub(crate) native: Option<String>,
}
impl From<(PropertyBinding, serde_json::Value)> for CommandProperty {
    fn from((binding, value): (PropertyBinding, serde_json::Value)) -> Self {
        Self {
            binding,
            value,
            path: None,
            native: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandStatus {
    pub(crate) id: u64,
    pub(crate) binding: PropertyBinding,
    pub(crate) target: CommandTarget,
    pub(crate) properties: Vec<CommandProperty>,
    pub(crate) actual_path: Option<PathKind>,
    pub(crate) result: Option<Result<(), CommandError>>,
    pub(crate) steps: Vec<CommandStep>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandStep {
    pub(crate) binding: PropertyBinding,
    pub(crate) path: PathKind,
    pub(crate) receipt: StepReceipt,
    pub(crate) native: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StepReceipt {
    Awaiting,
    Accepted,
    Rejected(String),
    Uncertain(String),
}

impl CommandStatus {
    pub(crate) fn properties_text(&self) -> String {
        if self.properties.is_empty() {
            return "none".into();
        }
        if matches!(self.target, CommandTarget::Power(_)) {
            let binding = self.properties[0].binding;
            return format!("{}.{}", binding.siid, binding.piid);
        }
        self.properties
            .iter()
            .map(|property| {
                let binding = property.binding;
                let value = &property.value;
                if let Some(native) = &property.native {
                    match property.path {
                        Some(path) => format!("{native} ({path})"),
                        None => native.clone(),
                    }
                } else if matches!(
                    self.target,
                    CommandTarget::Fan(_)
                        | CommandTarget::AirConditioner(_)
                        | CommandTarget::Heater(_)
                ) {
                    let path = property.path.expect("fan records contain submitted steps");
                    format!("{}.{}={value} ({path})", binding.siid, binding.piid)
                } else {
                    format!("{}.{}={value}", binding.siid, binding.piid)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    pub(crate) fn result_text(&self) -> String {
        match &self.result {
            None => "awaiting result".into(),
            Some(Ok(())) if self.properties.is_empty() => "unchanged (no write)".into(),
            Some(Ok(())) => "accepted".into(),
            Some(Err(CommandError::Timeout)) if self.actual_path.is_some() => {
                let message = "command acceptance timed out (result uncertain)";
                if matches!(self.target, CommandTarget::Power(_)) {
                    message.into()
                } else {
                    format!("failed: {message}")
                }
            }
            Some(Err(error)) if matches!(self.target, CommandTarget::Power(_)) => error.to_string(),
            Some(Err(CommandError::Superseded)) => "superseded".into(),
            Some(Err(CommandError::Stopped)) => "stopped".into(),
            Some(Err(CommandError::Cancelled)) => "cancelled".into(),
            Some(Err(error)) => format!("failed: {error}"),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelCommands {
    pub(crate) light_actions: Option<crate::device::LightActionDiagnostics>,
    pub(crate) channel: String,
    pub(crate) inflight: Option<CommandStatus>,
    pub(crate) last: Option<CommandStatus>,
}
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SubscriptionBinding {
    Property(PropertyBinding),
    Event(EventBinding),
}
impl From<PropertyBinding> for SubscriptionBinding {
    fn from(binding: PropertyBinding) -> Self {
        Self::Property(binding)
    }
}
impl fmt::Display for SubscriptionBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Property(binding) => write!(f, "property {}.{}", binding.siid, binding.piid),
            Self::Event(binding) => write!(f, "event {}.{}", binding.siid, binding.eiid),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SubscriptionStatus {
    pub(crate) binding: SubscriptionBinding,
    pub(crate) source: PathKind,
    pub(crate) ready: bool,
    pub(crate) problem: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NativeModeObservation {
    pub(crate) value: Option<u32>,
    pub(crate) source_time: Option<i64>,
    pub(crate) received_at: Option<i64>,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) struct NativeValueObservation {
    pub(super) value: serde_json::Value,
    pub(super) source_time: Option<i64>,
    pub(super) received_at: i64,
    pub(super) reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ObservedPropertyStatus {
    pub(crate) name: &'static str,
    pub(crate) binding: PropertyBinding,
    pub(crate) observation_source: Option<PathKind>,
    pub(crate) native_mode: Option<NativeModeObservation>,
    pub(crate) read_problem: Option<String>,
    pub(crate) source_detail: Option<String>,
    pub(crate) subscriptions: Vec<SubscriptionStatus>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LowSpeedStatus {
    /// Real operation candidates, never the hub.
    pub(crate) candidates: Vec<PathKind>,
    pub(crate) selected: Option<PathKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FunctionStatus {
    pub(crate) function: String,
    pub(crate) selected_read: Option<PathKind>,
    /// Present only for a fan that owns a shared low-speed operation.
    pub(crate) low_speed: Option<LowSpeedStatus>,
    pub(crate) notification_source: Option<PathKind>,
    pub(crate) sync_limited_reason: Option<String>,
    pub(crate) read_problem: Option<String>,
    pub(crate) observation_source: Option<PathKind>,
    pub(crate) subscriptions: Vec<SubscriptionStatus>,
    pub(crate) properties: Vec<ObservedPropertyStatus>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanState {
    Ineligible,
    MissingCredentials,
    InvalidCredentials,
    Authenticating,
    Ready,
    Failed,
}
pub(crate) use crate::xiaomi::capability::LanProtocol;
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanStatus {
    pub(crate) protocol: LanProtocol,
    pub(crate) state: LanState,
    pub(crate) address: Option<std::net::Ipv4Addr>,
    pub(crate) problem: Option<String>,
    pub(crate) recovery: RecoveryStatus,
}
/// One raw diagnostic input with its real source and observation order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticObservation {
    pub(crate) value: serde_json::Value,
    pub(crate) source: Option<PathKind>,
    pub(crate) source_time: Option<i64>,
    pub(crate) received_at: Option<i64>,
    pub(crate) reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SharedFanSpeedStatus {
    pub(crate) function: String,
    pub(crate) channel: crate::xiaomi::capability::SharedFanChannel,
    /// Logical level 1 = low, 2 = high; `None` is explicitly unknown.
    pub(crate) level: Option<u8>,
    pub(crate) reason: Option<String>,
    pub(crate) source_time: Option<i64>,
    pub(crate) received_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SharedFeedbackStatus {
    /// Current eligible read candidates, independent of the standard path.
    pub(crate) candidates: Vec<PathKind>,
    /// Current selected read path for the shared group; `None` when both
    /// direct and cloud are currently unavailable.
    pub(crate) selected_read: Option<PathKind>,
    /// Last successful observation source, retained when the current path is
    /// already unavailable or a later read failed.
    pub(crate) source: Option<PathKind>,
    pub(crate) pending: bool,
    pub(crate) read_problem: Option<String>,
    pub(crate) native_mode: Option<DiagnosticObservation>,
    pub(crate) fan_speed_index: Option<DiagnosticObservation>,
    pub(crate) temperature: Option<DiagnosticObservation>,
    pub(crate) fans: Vec<SharedFanSpeedStatus>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceStatus {
    pub(crate) did: String,
    pub(crate) lan: Option<LanStatus>,
    pub(crate) direct_unavailable_reason: Option<String>,
    pub(crate) read_only: bool,
    pub(crate) usable_control_paths: Vec<PathKind>,
    pub(crate) selected_control: Option<PathKind>,
    pub(crate) usable_read_paths: Vec<PathKind>,
    pub(crate) selected_read: Option<PathKind>,
    /// `None` means the source provides no reliable physical on/off signal.
    pub(crate) physical_online: Option<bool>,
    pub(crate) notification_source: Option<PathKind>,
    pub(crate) unavailable_reason: Option<String>,
    pub(crate) sync_limited_reason: Option<String>,
    pub(crate) battery_problem: Option<String>,
    pub(crate) last_switch_reason: Option<String>,
    pub(crate) standard_mode: Option<DiagnosticObservation>,
    pub(crate) shared_feedback: Option<SharedFeedbackStatus>,
    pub(crate) commands: Vec<ChannelCommands>,
    pub(crate) functions: Vec<FunctionStatus>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AdapterStatus {
    pub(crate) auth: AuthReport,
    pub(crate) certificate_status: Option<crate::xiaomi::certificate::CertificateStatus>,
    pub(crate) gateway: GatewayStatus,
    pub(crate) catalog_problem: Option<String>,
    pub(crate) pending: Vec<PendingDevice>,
    pub(crate) routing: RoutingStatus,
    pub(crate) devices: Vec<DeviceStatus>,
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
