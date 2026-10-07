use super::super::{Discovery, DiscoveryDriver};
use mdns_sd::{Receiver, ServiceEvent};

impl Discovery {
    pub(crate) fn with_events(receiver: Receiver<ServiceEvent>) -> (Self, DiscoveryDriver) {
        Self::from_events(receiver, None)
    }
}
