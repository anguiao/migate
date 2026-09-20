mod browser;
mod network;
mod platform;
mod profile;
mod registry;
mod socket;

pub use browser::DiscoveryBrowser;
pub use network::{
    DefaultRoute, InterfaceRecord, LinkType, NetworkEpoch, NetworkInterface, NetworkMonitor,
    NetworkSnapshot, NetworkTracker, NetworkUpdate, RouteGateway, TimeSample, WakeSample,
    capture_wake_sample, collect_network_snapshot,
};
pub use profile::GatewayProfile;
pub use registry::{
    DiscoveryRegistry, GatewayCandidate, GatewayEndpoint, MdnsEvent, RawResolvedService, ScopedIpv4,
};
pub use socket::LocalBinder;

pub const MIOT_SERVICE_TYPE: &str = "_miot-central._tcp.local.";

#[derive(Debug)]
pub struct DiscoveryError(String);

impl DiscoveryError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DiscoveryError {}

impl From<std::io::Error> for DiscoveryError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
