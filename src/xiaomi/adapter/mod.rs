mod bootstrap;
mod cloud;
mod commands;
mod credentials;
mod diagnostics;
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
    GatewayStatus, PathKind, PendingDevice, RecoveryStatus, RoutingStatus, TopologyReady,
};
pub use publication::SourceHandle;
pub use routing::PathPriority;
pub use runner::XiaomiAdapter;

#[cfg(test)]
mod tests;
