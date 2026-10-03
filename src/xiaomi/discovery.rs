use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    net::{SocketAddr, SocketAddrV6},
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use event_listener::Event;
use mdns_sd::{Receiver, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

pub const SERVICE_TYPE: &str = "_miot-central._tcp.local.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gateway {
    pub did: String,
    pub group_id: String,
    pub addresses: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayRoute {
    pub did: String,
    pub group_id: String,
    pub address: SocketAddr,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoverySnapshot {
    pub gateways: Vec<Gateway>,
    pub revision: u64,
}

impl DiscoverySnapshot {
    /// Retain an eligible current master and address. Backup announcements do
    /// not interrupt an authenticated route; replacement selection is stable.
    pub fn route(&self, group_id: &str, current: Option<&GatewayRoute>) -> Option<GatewayRoute> {
        let current = current.filter(|route| route.group_id == group_id);
        let current_gateway = current.and_then(|route| {
            self.gateways
                .iter()
                .find(|gateway| gateway.group_id == group_id && gateway.did == route.did)
        });
        let (gateway, address) = if let Some((gateway, route)) = current_gateway.zip(current) {
            let address = gateway
                .addresses
                .iter()
                .find(|address| **address == route.address)
                .or_else(|| gateway.addresses.iter().min())?;
            (gateway, *address)
        } else {
            let gateway = self
                .gateways
                .iter()
                .filter(|gateway| gateway.group_id == group_id && !gateway.addresses.is_empty())
                .min_by(|left, right| left.did.cmp(&right.did))?;
            (gateway, *gateway.addresses.iter().min()?)
        };
        Some(GatewayRoute {
            did: gateway.did.clone(),
            group_id: gateway.group_id.clone(),
            address,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryError {
    InvalidProfile,
    InvalidService,
    Backend(String),
    Stopped,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile => formatter.write_str("invalid local gateway profile"),
            Self::InvalidService => formatter.write_str("invalid local gateway service"),
            Self::Backend(error) => write!(formatter, "local gateway discovery failed: {error}"),
            Self::Stopped => formatter.write_str("local gateway discovery stopped"),
        }
    }
}

impl Error for DiscoveryError {}

#[derive(Default)]
struct State {
    snapshot: DiscoverySnapshot,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Event,
}

/// The current LAN directory. It never restores network addresses from storage.
#[derive(Clone)]
pub struct Discovery(Arc<Shared>);

impl Discovery {
    pub fn start() -> Result<(Self, DiscoveryDriver), DiscoveryError> {
        let daemon =
            ServiceDaemon::new().map_err(|error| DiscoveryError::Backend(error.to_string()))?;
        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(receiver) => receiver,
            Err(error) => {
                shutdown_daemon(&daemon);
                return Err(DiscoveryError::Backend(error.to_string()));
            }
        };
        Ok(Self::from_events(
            receiver,
            Some(Box::new(move || shutdown_daemon(&daemon))),
        ))
    }

    fn from_events(
        receiver: Receiver<ServiceEvent>,
        shutdown: Option<Box<dyn FnOnce() + Send>>,
    ) -> (Self, DiscoveryDriver) {
        let discovery = Self(Arc::new(Shared::default()));
        let driver = DiscoveryDriver {
            discovery: discovery.clone(),
            receiver,
            registry: Registry::default(),
            shutdown,
        };
        (discovery, driver)
    }

    #[cfg(test)]
    pub(crate) fn with_events(receiver: Receiver<ServiceEvent>) -> (Self, DiscoveryDriver) {
        Self::from_events(receiver, None)
    }

    pub fn snapshot(&self) -> DiscoverySnapshot {
        self.0.state.lock().unwrap().snapshot.clone()
    }

    pub fn subscribe(&self) -> DiscoveryChanges {
        DiscoveryChanges {
            discovery: self.clone(),
            revision: self.snapshot().revision,
        }
    }

    fn publish(&self, snapshot: DiscoverySnapshot) {
        let mut state = self.0.state.lock().unwrap();
        if state.snapshot.gateways != snapshot.gateways {
            state.snapshot.gateways = snapshot.gateways;
            state.snapshot.revision = state.snapshot.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
}

pub struct DiscoveryChanges {
    discovery: Discovery,
    revision: u64,
}

impl DiscoveryChanges {
    /// Changes can coalesce. Dropping this wait does not consume a revision.
    pub async fn changed(&mut self) -> Result<DiscoverySnapshot, DiscoveryError> {
        loop {
            let listener = self.discovery.0.changed.listen();
            {
                let state = self.discovery.0.state.lock().unwrap();
                if state.stopped {
                    return Err(DiscoveryError::Stopped);
                }
                if state.snapshot.revision != self.revision {
                    self.revision = state.snapshot.revision;
                    return Ok(state.snapshot.clone());
                }
            }
            listener.await;
        }
    }
}

/// Consume all daemon updates for the full service lifetime, including TXT,
/// address changes and removals. First-start waiting belongs to the caller.
pub struct DiscoveryDriver {
    discovery: Discovery,
    receiver: Receiver<ServiceEvent>,
    registry: Registry,
    shutdown: Option<Box<dyn FnOnce() + Send>>,
}

impl DiscoveryDriver {
    pub async fn run(mut self) -> Result<(), DiscoveryError> {
        loop {
            let event = self
                .receiver
                .recv_async()
                .await
                .map_err(|_| DiscoveryError::Stopped)?;
            match event {
                ServiceEvent::ServiceResolved(service) if service.ty_domain == SERVICE_TYPE => {
                    if let Err(error) = self.registry.resolve(*service) {
                        log::warn!("Gateway discovery rejected an announcement: {error}");
                    }
                }
                ServiceEvent::ServiceRemoved(service_type, name)
                    if service_type == SERVICE_TYPE =>
                {
                    self.registry.services.remove(&name.to_ascii_lowercase());
                }
                ServiceEvent::SearchStopped(service_type) if service_type == SERVICE_TYPE => {
                    return Err(DiscoveryError::Stopped);
                }
                _ => continue,
            }
            self.discovery.publish(self.registry.snapshot());
        }
    }
}

impl Drop for DiscoveryDriver {
    fn drop(&mut self) {
        let mut state = self.discovery.0.state.lock().unwrap();
        state.stopped = true;
        state.snapshot.gateways.clear();
        state.snapshot.revision = state.snapshot.revision.wrapping_add(1);
        self.discovery.0.changed.notify(usize::MAX);
        drop(state);
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
    }
}

fn shutdown_daemon(daemon: &ServiceDaemon) {
    if let Err(error) = daemon.stop_browse(SERVICE_TYPE) {
        log::warn!("Failed to stop gateway discovery: {error}");
    }
    if let Err(error) = daemon.shutdown() {
        log::warn!("Failed to stop gateway discovery daemon: {error}");
    }
}

#[derive(Default)]
struct Registry {
    services: BTreeMap<String, Announcement>,
}

impl Registry {
    fn resolve(&mut self, service: ResolvedService) -> Result<(), DiscoveryError> {
        let name = service.fullname.to_ascii_lowercase();
        // Replacement is authoritative even when the new profile is unusable.
        self.services.remove(&name);
        if service.ty_domain != SERVICE_TYPE || service.port == 0 || !service.is_valid() {
            return Err(DiscoveryError::InvalidService);
        }
        let profile = parse_profile(
            service
                .get_property_val_str("profile")
                .ok_or(DiscoveryError::InvalidProfile)?,
        )?;
        let mut addresses = service
            .addresses
            .iter()
            .filter_map(|address| socket_address(address, service.port))
            .collect::<Vec<_>>();
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            return Err(DiscoveryError::InvalidService);
        }
        self.services
            .insert(name, Announcement { profile, addresses });
        Ok(())
    }

    fn snapshot(&self) -> DiscoverySnapshot {
        let mut gateways: BTreeMap<(&str, &str), Vec<SocketAddr>> = BTreeMap::new();
        for announcement in self
            .services
            .values()
            .filter(|announcement| announcement.profile.is_master())
        {
            gateways
                .entry((&announcement.profile.group_id, &announcement.profile.did))
                .or_default()
                .extend(&announcement.addresses);
        }
        DiscoverySnapshot {
            gateways: gateways
                .into_iter()
                .map(|((group_id, did), mut addresses)| {
                    addresses.sort_unstable();
                    addresses.dedup();
                    Gateway {
                        did: did.into(),
                        group_id: group_id.into(),
                        addresses,
                    }
                })
                .collect(),
            revision: 0,
        }
    }
}

struct Announcement {
    profile: Profile,
    addresses: Vec<SocketAddr>,
}

struct Profile {
    did: String,
    group_id: String,
    role: u8,
    mqtt: bool,
}

impl Profile {
    fn is_master(&self) -> bool {
        self.role == 1 && self.mqtt
    }
}

fn parse_profile(value: &str) -> Result<Profile, DiscoveryError> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| DiscoveryError::InvalidProfile)?;
    if bytes.len() < 23 {
        return Err(DiscoveryError::InvalidProfile);
    }
    Ok(Profile {
        did: u64::from_be_bytes(bytes[1..9].try_into().unwrap()).to_string(),
        group_id: bytes[9..17]
            .iter()
            .rev()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        role: bytes[20] >> 4,
        mqtt: bytes[22] & 2 != 0,
    })
}

fn socket_address(address: &ScopedIp, port: u16) -> Option<SocketAddr> {
    match address {
        ScopedIp::V4(address) => Some(SocketAddr::new((*address.addr()).into(), port)),
        ScopedIp::V6(address) => {
            let scope = address.scope_id().index;
            if address.addr().is_unicast_link_local() && scope == 0 {
                return None;
            }
            Some(SocketAddrV6::new(*address.addr(), port, 0, scope).into())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
