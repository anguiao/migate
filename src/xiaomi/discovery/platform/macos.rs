use std::{collections::BTreeMap, ffi::CStr, process::Stdio, time::Duration};

use async_io::Timer;
use async_process::Command;
use futures_lite::future;

use super::{DiscoveryError, InterfaceRecord, LinkMetadata, LinkType};
use crate::xiaomi::discovery::{DefaultRoute, RouteGateway, WakeSample};

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

#[cfg(target_os = "macos")]
pub(super) fn link_metadata(
    _interfaces: &[if_addrs::Interface],
) -> Result<BTreeMap<u32, LinkMetadata>, DiscoveryError> {
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
                LinkType::Other(u16::from(link.sdl_type))
            };
            // SAFETY: getifaddrs supplies a null-terminated interface name.
            let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_bytes();
            types.insert(
                u32::from(link.sdl_index),
                LinkMetadata {
                    link_type,
                    physical: name.starts_with(b"en") && link_type == LinkType::Ethernet,
                },
            );
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: head is the list returned by getifaddrs above.
    unsafe { libc::freeifaddrs(head) };
    Ok(types)
}

pub async fn collect_default_route(
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
        return Err(DiscoveryError::new(format!(
            "default route lookup failed: {}",
            error_output.trim()
        )));
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
mod tests;
