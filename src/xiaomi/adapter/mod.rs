mod bootstrap;
mod cloud;
mod commands;
mod credentials;
mod diagnostics;
mod group_rules;
mod groups;
mod initial_reads;
mod lan;
mod legacy_miio;
mod lighting;
mod local;
mod mcn04;
mod membership;
mod model;
mod network;
mod notifications;
mod observations;
pub(crate) mod publication;
mod refresh;
mod relations;
mod routing;
mod runner;
mod splits;
mod state;
mod tasks;
mod work;

pub use model::{
    AdapterError, AdapterStatus, ChannelCommands, CommandProperty, CommandStatus, CommandStep,
    CommandTarget, DeviceStatus, DiagnosticObservation, FunctionStatus, GatewayStatus, LanProtocol,
    LanState, LanStatus, NativeValueObservation, ObservedPropertyStatus, PathKind, PendingDevice,
    RecoveryStatus, RoutingStatus, SharedFanSpeedStatus, SharedFeedbackStatus, StepReceipt,
    SubscriptionBinding, SubscriptionStatus, TopologyReady,
};
pub use publication::SourceHandle;
pub use routing::PathPriority;
pub use runner::XiaomiAdapter;

#[cfg(test)]
pub(crate) mod tests;
