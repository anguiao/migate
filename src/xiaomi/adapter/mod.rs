mod bootstrap;
mod cloud;
mod commands;
mod credentials;
mod diagnostics;
mod directory;
mod group_rules;
mod groups;
mod hub;
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
mod pending;
pub(crate) mod publication;
mod refresh;
mod relations;
mod routing;
mod runner;
mod splits;
mod state;
mod tasks;
mod work;

pub use model::{AdapterError, AdapterStatus, TopologyReady};
pub(crate) use model::{
    ChannelCommands, CommandStatus, CommandTarget, DeviceStatus, DiagnosticObservation,
    FunctionStatus, GatewayStatus, LanProtocol, LanState, ObservedPropertyStatus, PathKind,
    PendingDevice, RecoveryStatus, RoutingStatus, StepReceipt,
};
pub use publication::SourceHandle;
pub(crate) use routing::PathPriority;
pub use runner::XiaomiAdapter;

#[cfg(test)]
pub(crate) mod tests;
