use std::net::Ipv4Addr;

use super::{DiscoveryError, InterfaceRecord};
use crate::xiaomi::discovery::{DefaultRoute, RouteGateway};

#[cfg(target_os = "linux")]
pub fn capture_wake_sample() -> Result<crate::xiaomi::discovery::WakeSample, DiscoveryError> {
    fn millis(clock: libc::clockid_t) -> Result<u64, DiscoveryError> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: time points to initialized writable storage; clock is a Linux clock ID.
        if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(time.tv_sec as u64 * 1_000 + time.tv_nsec as u64 / 1_000_000)
    }
    Ok(crate::xiaomi::discovery::WakeSample {
        active_millis: millis(libc::CLOCK_MONOTONIC)?,
        continuous_millis: millis(libc::CLOCK_BOOTTIME)?,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn link_metadata(
    interfaces: &[if_addrs::Interface],
) -> Result<std::collections::BTreeMap<u32, super::LinkMetadata>, DiscoveryError> {
    let mut links = std::collections::BTreeMap::new();
    for interface in interfaces {
        let Some(index) = interface.index else {
            continue;
        };
        if links.contains_key(&index) {
            continue;
        }
        links.insert(
            index,
            interface_link(std::path::Path::new("/sys"), &interface.name)?,
        );
    }
    Ok(links)
}

fn interface_link(
    sysfs: &std::path::Path,
    name: &str,
) -> Result<super::LinkMetadata, DiscoveryError> {
    let path = sysfs.join("class/net").join(name);
    let link_type: u16 = std::fs::read_to_string(path.join("type"))?
        .trim()
        .parse()
        .map_err(|_| DiscoveryError::new("invalid Linux interface link type"))?;
    let link_type = if link_type == 1 {
        super::LinkType::Ethernet
    } else {
        super::LinkType::Other(link_type)
    };
    // Hardware-backed Ethernet and Wi-Fi have a device link. Bridges, veth,
    // tun/tap and other software interfaces live under /sys/devices/virtual.
    let physical = path.join("device").try_exists()?
        && !path
            .canonicalize()?
            .starts_with(sysfs.join("devices/virtual"));
    Ok(super::LinkMetadata {
        link_type,
        physical,
    })
}

#[cfg(target_os = "linux")]
pub async fn collect_default_route(
    records: &[InterfaceRecord],
    _timeout: std::time::Duration,
) -> Result<Option<DefaultRoute>, DiscoveryError> {
    parse_default_route(records, &std::fs::read_to_string("/proc/net/route")?)
}

fn parse_default_route(
    records: &[InterfaceRecord],
    output: &str,
) -> Result<Option<DefaultRoute>, DiscoveryError> {
    let mut lines = output.lines();
    if !lines.next().is_some_and(|header| {
        header
            .split_whitespace()
            .take(3)
            .eq(["Iface", "Destination", "Gateway"])
    }) {
        return Err(DiscoveryError::new("invalid Linux route table header"));
    }
    let mut selected = None;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 11 {
            return Err(DiscoveryError::new("invalid Linux route record"));
        }
        if fields[1] != "00000000" || fields[7] != "00000000" {
            continue;
        }
        let flags = u16::from_str_radix(fields[3], 16)
            .map_err(|_| DiscoveryError::new("invalid Linux route flags"))?;
        const RTF_UP: u16 = 0x1;
        const RTF_GATEWAY: u16 = 0x2;
        const RTF_REJECT: u16 = 0x200;
        if flags & RTF_UP == 0 || flags & RTF_REJECT != 0 {
            continue;
        }
        let metric: u32 = fields[6]
            .parse()
            .map_err(|_| DiscoveryError::new("invalid Linux route metric"))?;
        if selected
            .as_ref()
            .is_some_and(|(best, _, _)| *best <= metric)
        {
            continue;
        }
        let gateway = if flags & RTF_GATEWAY == 0 {
            RouteGateway::Interface
        } else {
            let address = u32::from_str_radix(fields[2], 16)
                .map_err(|_| DiscoveryError::new("invalid Linux route gateway"))?;
            // procfs prints the network-order address as a native-endian integer.
            RouteGateway::Ipv4(Ipv4Addr::from(address.to_ne_bytes()))
        };
        selected = Some((metric, fields[0], gateway));
    }
    selected
        .map(|(_, name, gateway)| {
            let interface_index = records
                .iter()
                .find(|record| record.name == name)
                .map(|record| record.index)
                .ok_or_else(|| DiscoveryError::new("default route interface was not found"))?;
            Ok(DefaultRoute {
                interface_index,
                gateway,
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests;
