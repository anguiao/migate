use std::{
    net::Ipv4Addr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{DiscoveryError, platform};
pub use platform::capture_wake_sample;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkType {
    Ethernet,
    Other(u16),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterfaceRecord {
    pub index: u32,
    pub name: String,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub up: bool,
    pub point_to_point: bool,
    pub loopback: bool,
    pub link_type: LinkType,
    pub physical: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct NetworkInterface {
    pub(crate) index: u32,
    pub(crate) name: String,
    pub(crate) address: Ipv4Addr,
    pub(crate) netmask: Ipv4Addr,
    pub(crate) prefix_len: u8,
}

impl NetworkInterface {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn address(&self) -> Ipv4Addr {
        self.address
    }

    pub fn netmask(&self) -> Ipv4Addr {
        self.netmask
    }

    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }

    pub fn on_link(&self, target: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        u32::from(self.address) & mask == u32::from(target) & mask
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefaultRoute {
    pub interface_index: u32,
    pub gateway: RouteGateway,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteGateway {
    Ipv4(Ipv4Addr),
    Link(u32),
    Interface,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkSnapshot {
    interfaces: Vec<NetworkInterface>,
    default_route: Option<DefaultRoute>,
}

impl NetworkSnapshot {
    pub fn select(
        records: Vec<InterfaceRecord>,
        default_route: Option<DefaultRoute>,
    ) -> Result<Self, DiscoveryError> {
        let mut interfaces = records
            .into_iter()
            .filter(is_physical_ipv4)
            .map(|record| {
                let prefix_len = prefix_len(record.netmask)?;
                Ok(NetworkInterface {
                    index: record.index,
                    name: record.name,
                    address: record.address,
                    netmask: record.netmask,
                    prefix_len,
                })
            })
            .collect::<Result<Vec<_>, DiscoveryError>>()?;
        interfaces.sort();
        interfaces.dedup();

        if interfaces.is_empty() {
            return Err(DiscoveryError::new(
                "no physical IPv4 interface is available",
            ));
        }

        Ok(Self {
            interfaces,
            default_route,
        })
    }

    pub fn interfaces(&self) -> &[NetworkInterface] {
        &self.interfaces
    }

    pub fn interface(&self, index: u32) -> Option<&NetworkInterface> {
        self.interfaces
            .iter()
            .find(|interface| interface.index == index)
    }

    pub fn interfaces_with_index(&self, index: u32) -> impl Iterator<Item = &NetworkInterface> {
        self.interfaces
            .iter()
            .filter(move |interface| interface.index == index)
    }

    pub fn default_route(&self) -> Option<&DefaultRoute> {
        self.default_route.as_ref()
    }
}

fn is_physical_ipv4(record: &InterfaceRecord) -> bool {
    record.up
        && !record.point_to_point
        && !record.loopback
        && record.link_type == LinkType::Ethernet
        && record.index != 0
        && record.physical
        && !record.address.is_unspecified()
        && !record.address.is_loopback()
        && !record.address.is_multicast()
}

fn prefix_len(netmask: Ipv4Addr) -> Result<u8, DiscoveryError> {
    let value = u32::from(netmask);
    let length = value.leading_ones() as u8;
    let expected = u32::MAX.checked_shl(u32::from(32 - length)).unwrap_or(0);
    if value != expected {
        return Err(DiscoveryError::new("interface netmask is not contiguous"));
    }
    Ok(length)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct NetworkEpoch(u64);

impl NetworkEpoch {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeSample {
    pub monotonic_millis: u64,
    pub wall_unix_millis: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeSample {
    pub active_millis: u64,
    pub continuous_millis: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkUpdate {
    pub epoch: NetworkEpoch,
    pub snapshot: NetworkSnapshot,
}

pub struct NetworkTracker {
    current: Option<NetworkUpdate>,
    last_time: Option<TimeSample>,
    last_wake: Option<WakeSample>,
}

impl NetworkTracker {
    pub fn new() -> Self {
        Self {
            current: None,
            last_time: None,
            last_wake: None,
        }
    }

    pub fn observe(&mut self, snapshot: NetworkSnapshot, time: TimeSample) -> NetworkUpdate {
        self.observe_with_wake(snapshot, time, false)
    }

    pub fn observe_with_wake(
        &mut self,
        snapshot: NetworkSnapshot,
        time: TimeSample,
        woke_from_sleep: bool,
    ) -> NetworkUpdate {
        let topology_changed = self
            .current
            .as_ref()
            .is_some_and(|current| current.snapshot != snapshot);
        let resumed = woke_from_sleep
            || self.last_time.is_some_and(|previous| {
                let monotonic_delta = time
                    .monotonic_millis
                    .saturating_sub(previous.monotonic_millis);
                let wall_delta = time
                    .wall_unix_millis
                    .saturating_sub(previous.wall_unix_millis);
                wall_delta > monotonic_delta as i64 + 5_000
                    || time.wall_unix_millis < previous.wall_unix_millis
            });
        let next_epoch = match &self.current {
            None => NetworkEpoch::new(1),
            Some(current) if topology_changed || resumed => {
                NetworkEpoch::new(current.epoch.get().saturating_add(1))
            }
            Some(current) => current.epoch,
        };
        let update = NetworkUpdate {
            epoch: next_epoch,
            snapshot,
        };
        self.current = Some(update.clone());
        self.last_time = Some(time);
        update
    }

    pub fn observe_with_wake_sample(
        &mut self,
        snapshot: NetworkSnapshot,
        time: TimeSample,
        wake: WakeSample,
    ) -> NetworkUpdate {
        let woke_from_sleep = self.last_wake.is_some_and(|previous| {
            let active_delta = wake.active_millis.saturating_sub(previous.active_millis);
            let continuous_delta = wake
                .continuous_millis
                .saturating_sub(previous.continuous_millis);
            continuous_delta > active_delta.saturating_add(50)
        });
        self.last_wake = Some(wake);
        self.observe_with_wake(snapshot, time, woke_from_sleep)
    }

    pub fn invalidate(&mut self) -> NetworkEpoch {
        let next_epoch = self
            .current
            .as_ref()
            .map_or(NetworkEpoch::new(1), |current| {
                NetworkEpoch::new(current.epoch.get().saturating_add(1))
            });
        if let Some(current) = &mut self.current {
            current.epoch = next_epoch;
        }
        self.last_time = None;
        self.last_wake = None;
        next_epoch
    }
}

impl Default for NetworkTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NetworkMonitor {
    tracker: NetworkTracker,
}

impl NetworkMonitor {
    pub fn new() -> Self {
        Self {
            tracker: NetworkTracker::new(),
        }
    }

    pub async fn refresh(
        &mut self,
        route_timeout: Duration,
    ) -> Result<NetworkUpdate, DiscoveryError> {
        let wake = capture_wake_sample()?;
        let wall_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| DiscoveryError::new("system wall clock is before Unix epoch"))?
            .as_millis()
            .try_into()
            .map_err(|_| DiscoveryError::new("system wall clock is out of range"))?;
        let snapshot = match collect_network_snapshot(route_timeout).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.tracker.invalidate();
                return Err(error);
            }
        };
        Ok(self.tracker.observe_with_wake_sample(
            snapshot,
            TimeSample {
                monotonic_millis: wake.active_millis,
                wall_unix_millis,
            },
            wake,
        ))
    }
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn collect_network_snapshot(
    timeout: Duration,
) -> Result<NetworkSnapshot, DiscoveryError> {
    let records = platform::collect_interface_records()?;
    let default_route = platform::collect_default_route(&records, timeout).await?;
    NetworkSnapshot::select(records, default_route)
}
