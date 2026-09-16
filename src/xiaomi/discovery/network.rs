use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_io::Timer;
use async_process::Command;
use futures_lite::future;
use if_addrs::{IfAddr, get_if_addrs};

use super::DiscoveryError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkType {
    Ethernet,
    Other(u8),
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
    const EXCLUDED_PREFIXES: &[&str] = &[
        "lo", "utun", "tun", "tap", "p2p", "bridge", "awdl", "llw", "gif", "stf",
    ];
    record.up
        && !record.point_to_point
        && !record.loopback
        && record.link_type == LinkType::Ethernet
        && record.index != 0
        && record.name.starts_with("en")
        && !EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| record.name.starts_with(prefix))
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

#[cfg(target_os = "macos")]
pub fn capture_wake_sample() -> Result<WakeSample, DiscoveryError> {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }
    let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: timebase points to initialized writable storage.
    if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
        return Err(DiscoveryError::new("cannot read macOS wake clock"));
    }
    let to_millis = |ticks: u64| {
        (u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom) / 1_000_000)
            as u64
    };
    Ok(WakeSample {
        // SAFETY: both functions are side-effect-free Darwin monotonic clock reads.
        active_millis: to_millis(unsafe { mach_absolute_time() }),
        // SAFETY: mach_continuous_time is available on supported macOS versions.
        continuous_millis: to_millis(unsafe { mach_continuous_time() }),
    })
}

#[cfg(not(target_os = "macos"))]
pub fn capture_wake_sample() -> Result<WakeSample, DiscoveryError> {
    Err(DiscoveryError::new(
        "wake clock collection is supported only on macOS",
    ))
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
    let records = collect_interface_records()?;
    let default_route = collect_default_route(&records, timeout).await?;
    NetworkSnapshot::select(records, default_route)
}

fn collect_interface_records() -> Result<Vec<InterfaceRecord>, DiscoveryError> {
    let link_types = link_types_by_index()?;
    let records = get_if_addrs()
        .map_err(DiscoveryError::from)?
        .into_iter()
        .filter_map(|interface| {
            let IfAddr::V4(ref ipv4) = interface.addr else {
                return None;
            };
            let index = interface.index?;
            let link_type = link_types
                .get(&index)
                .copied()
                .unwrap_or(LinkType::Other(0));
            let up = interface.is_oper_up();
            let point_to_point = interface.is_p2p();
            Some(InterfaceRecord {
                index,
                name: interface.name,
                address: ipv4.ip,
                netmask: ipv4.netmask,
                up,
                point_to_point,
                loopback: ipv4.ip.is_loopback(),
                link_type,
            })
        })
        .collect::<Vec<_>>();
    Ok(records)
}

#[cfg(target_os = "macos")]
fn link_types_by_index() -> Result<BTreeMap<u32, LinkType>, DiscoveryError> {
    let mut head = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes `head` on success and freeifaddrs accepts that list.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(DiscoveryError::from(std::io::Error::last_os_error()));
    }
    let mut types = BTreeMap::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: cursor belongs to the live getifaddrs list.
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null()
            // SAFETY: the address points to a sockaddr whose family can be read.
            && unsafe { (*entry.ifa_addr).sa_family as i32 } == libc::AF_LINK
        {
            // SAFETY: AF_LINK addresses use sockaddr_dl on Darwin.
            let link = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_dl>()) };
            const IFT_ETHER: u8 = 6;
            let link_type = if link.sdl_type == IFT_ETHER {
                LinkType::Ethernet
            } else {
                LinkType::Other(link.sdl_type)
            };
            types.insert(u32::from(link.sdl_index), link_type);
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: head is the list returned by getifaddrs above.
    unsafe { libc::freeifaddrs(head) };
    Ok(types)
}

#[cfg(not(target_os = "macos"))]
fn link_types_by_index() -> Result<BTreeMap<u32, LinkType>, DiscoveryError> {
    Ok(BTreeMap::new())
}

async fn collect_default_route(
    records: &[InterfaceRecord],
    timeout: Duration,
) -> Result<Option<DefaultRoute>, DiscoveryError> {
    let output = future::race(
        async {
            let mut command = Command::new("/sbin/route");
            command
                .args(["-n", "get", "-inet", "default"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            command.output().await.map_err(|error| {
                DiscoveryError::new(format!("cannot inspect default route: {error}"))
            })
        },
        async {
            Timer::after(timeout).await;
            Err(DiscoveryError::new("default route lookup timed out"))
        },
    )
    .await?;
    let error_output = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if error_output.contains("not in table") || error_output.contains("not found") {
            return Ok(None);
        }
        return Err(DiscoveryError::new("default route lookup failed"));
    }
    let route_output = std::str::from_utf8(&output.stdout)
        .map_err(|_| DiscoveryError::new("default route output is not UTF-8"))?;
    parse_default_route(records, route_output).map(Some)
}

fn parse_default_route(
    records: &[InterfaceRecord],
    output: &str,
) -> Result<DefaultRoute, DiscoveryError> {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.trim().split_once(':')?;
            (key == name).then(|| value.trim())
        })
    };
    let interface_name =
        field("interface").ok_or_else(|| DiscoveryError::new("default route has no interface"))?;
    let interface_index = records
        .iter()
        .find(|record| record.name == interface_name)
        .map(|record| record.index)
        .ok_or_else(|| DiscoveryError::new("default route interface was not found"))?;
    let gateway = match field("gateway") {
        Some(gateway_text) if gateway_text.starts_with("link#") => RouteGateway::Link(
            gateway_text[5..]
                .parse()
                .map_err(|_| DiscoveryError::new("default route link is invalid"))?,
        ),
        Some(gateway_text) => RouteGateway::Ipv4(
            gateway_text
                .parse()
                .map_err(|_| DiscoveryError::new("default route gateway is invalid"))?,
        ),
        None => RouteGateway::Interface,
    };
    Ok(DefaultRoute {
        interface_index,
        gateway,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_interface_default_route_does_not_require_a_gateway_field() {
        let records = vec![InterfaceRecord {
            index: 7,
            name: "utun4".into(),
            address: Ipv4Addr::new(10, 0, 0, 2),
            netmask: Ipv4Addr::new(255, 0, 0, 0),
            up: true,
            point_to_point: true,
            loopback: false,
            link_type: LinkType::Other(0),
        }];
        let route = parse_default_route(
            &records,
            "destination: default\nmask: default\ninterface: utun4\nflags: <UP,DONE>\n",
        )
        .unwrap();
        assert_eq!(route.interface_index, 7);
        assert_eq!(route.gateway, RouteGateway::Interface);
    }
}
