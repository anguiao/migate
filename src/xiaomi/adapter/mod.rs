mod bootstrap;
mod cloud;
mod commands;
mod credentials;
mod diagnostics;
mod group_rules;
mod groups;
mod lighting;
mod local;
mod membership;
mod model;
mod notifications;
mod observations;
pub(crate) mod publication;
mod refresh;
mod relations;
mod routing;
mod runner;
mod state;
mod tasks;
mod work;

pub use model::{
    AdapterError, AdapterStatus, ChannelCommands, CommandStatus, CommandTarget, DeviceStatus,
    FunctionStatus, GatewayStatus, ObservedPropertyStatus, PathKind, PendingDevice, RecoveryStatus,
    RoutingStatus, SubscriptionBinding, SubscriptionStatus, TopologyReady,
};
pub use publication::SourceHandle;
pub use routing::PathPriority;
pub use runner::XiaomiAdapter;

#[cfg(test)]
pub(crate) mod tests;
