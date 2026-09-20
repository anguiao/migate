use if_addrs::{IfAddr, get_if_addrs};

use super::{DiscoveryError, InterfaceRecord, LinkType};

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as system;
#[cfg(target_os = "macos")]
use macos as system;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("MiGate currently supports macOS and Linux");

pub use system::capture_wake_sample;
pub(super) use system::collect_default_route;

#[derive(Clone, Copy)]
struct LinkMetadata {
    link_type: LinkType,
    physical: bool,
}

pub(super) fn collect_interface_records() -> Result<Vec<InterfaceRecord>, DiscoveryError> {
    let interfaces = get_if_addrs()?;
    let links = system::link_metadata(&interfaces)?;
    Ok(interfaces
        .into_iter()
        .filter_map(|interface| {
            let IfAddr::V4(ref ipv4) = interface.addr else {
                return None;
            };
            let index = interface.index?;
            let metadata = links.get(&index)?;
            Some(InterfaceRecord {
                index,
                name: interface.name.clone(),
                address: ipv4.ip,
                netmask: ipv4.netmask,
                up: interface.is_oper_up(),
                point_to_point: interface.is_p2p(),
                loopback: interface.is_loopback(),
                link_type: metadata.link_type,
                physical: metadata.physical,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests;
