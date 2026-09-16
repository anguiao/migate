use crate::xiaomi::discovery::{LocalBinder, NetworkEpoch, NetworkInterface};
use async_io::{Async, Timer};
use futures_lite::future;
use futures_util::future::select_all;
use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    time::{Duration, Instant},
};

use super::{
    LanError, LanErrorKind, LanSendGuard,
    packet::{LEGACY_PROBE, native_probe},
};

const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CANDIDATES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanHelloCandidate {
    pub did: u64,
    pub address: SocketAddrV4,
    pub interface_index: u32,
    pub epoch: NetworkEpoch,
    pub timestamp_hint: u32,
    pub native_hint: bool,
    pub subscription_hint: Option<LanSubscriptionHint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LanSubscriptionHint {
    pub subscription_timestamp: u32,
    pub subscription_type: u8,
    pub wildcard_supported: bool,
}

pub(super) struct ParsedHello {
    pub did: u64,
    pub timestamp: u32,
    pub subscription_hint: Option<LanSubscriptionHint>,
}

pub struct LanDiscovery {
    virtual_did: u64,
    last_probe: Option<Instant>,
}

impl LanDiscovery {
    pub fn new(virtual_did: u64) -> Result<Self, LanError> {
        if virtual_did == 0 {
            return Err(LanError::new(
                "configure LAN discovery",
                LanErrorKind::InvalidInput,
            ));
        }
        Ok(Self {
            virtual_did,
            last_probe: None,
        })
    }

    pub async fn probe(
        &mut self,
        interfaces: &[NetworkInterface],
        epoch: NetworkEpoch,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<LanHelloCandidate>, LanError> {
        if interfaces.is_empty() || deadline <= Instant::now() {
            return Err(LanError::new("probe LAN", LanErrorKind::InvalidInput));
        }
        if self
            .last_probe
            .is_some_and(|previous| previous.elapsed() < PROBE_INTERVAL)
        {
            return Err(LanError::new("probe LAN", LanErrorKind::RateLimited));
        }
        self.last_probe = Some(Instant::now());
        let mut sockets = Vec::with_capacity(interfaces.len());
        for interface in interfaces {
            let socket = LocalBinder::new(interface.clone())
                .udp_socket()
                .map_err(|_| LanError::new("bind LAN probe", LanErrorKind::Transport))?;
            let broadcast = directed_broadcast(interface);
            send_probes(&socket, broadcast, self.virtual_did, deadline, &guard).await?;
            sockets.push((interface.clone(), socket));
        }
        collect_candidates(&sockets, epoch, deadline, true).await
    }

    pub async fn probe_address(
        &mut self,
        interface: NetworkInterface,
        epoch: NetworkEpoch,
        address: SocketAddrV4,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<LanHelloCandidate>, LanError> {
        if address.port() != 54321
            || !valid_candidate_address(*address.ip(), &interface)
            || deadline <= Instant::now()
        {
            return Err(LanError::new(
                "probe LAN address",
                LanErrorKind::InvalidInput,
            ));
        }
        if self
            .last_probe
            .is_some_and(|previous| previous.elapsed() < PROBE_INTERVAL)
        {
            return Err(LanError::new(
                "probe LAN address",
                LanErrorKind::RateLimited,
            ));
        }
        self.last_probe = Some(Instant::now());
        let socket = LocalBinder::new(interface.clone())
            .udp_socket()
            .map_err(|_| LanError::new("bind LAN probe", LanErrorKind::Transport))?;
        send_probes(&socket, address, self.virtual_did, deadline, &guard).await?;
        collect_candidates(&[(interface, socket)], epoch, deadline, true).await
    }

    #[cfg(test)]
    pub(super) async fn probe_socket(
        &mut self,
        interface: NetworkInterface,
        epoch: NetworkEpoch,
        socket: Async<UdpSocket>,
        destination: SocketAddrV4,
        deadline: Instant,
        guard: LanSendGuard,
    ) -> Result<Vec<LanHelloCandidate>, LanError> {
        if self
            .last_probe
            .is_some_and(|previous| previous.elapsed() < PROBE_INTERVAL)
        {
            return Err(LanError::new("probe LAN", LanErrorKind::RateLimited));
        }
        self.last_probe = Some(Instant::now());
        send_probes(&socket, destination, self.virtual_did, deadline, &guard).await?;
        collect_candidates(&[(interface, socket)], epoch, deadline, false).await
    }
}

async fn send_probes(
    socket: &Async<UdpSocket>,
    destination: SocketAddrV4,
    virtual_did: u64,
    deadline: Instant,
    guard: &LanSendGuard,
) -> Result<(), LanError> {
    for packet in [&native_probe(virtual_did), &LEGACY_PROBE] {
        loop {
            let cancelled = guard.cancelled_signal();
            if !guard.check()? {
                return Err(LanError::new("send LAN probe", LanErrorKind::Cancelled));
            }
            if Instant::now() >= deadline {
                return Err(LanError::new("send LAN probe", LanErrorKind::Timeout));
            }
            match socket.get_ref().send_to(packet, destination) {
                Ok(written) if written == packet.len() => break,
                Ok(_) => return Err(LanError::new("send LAN probe", LanErrorKind::Transport)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    enum Wake {
                        Writable(std::io::Result<()>),
                        Deadline,
                        Cancelled,
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match future::or(
                        async { Wake::Writable(socket.writable().await) },
                        future::or(
                            async {
                                Timer::after(remaining).await;
                                Wake::Deadline
                            },
                            async {
                                cancelled.await;
                                Wake::Cancelled
                            },
                        ),
                    )
                    .await
                    {
                        Wake::Writable(Ok(())) => {}
                        Wake::Writable(Err(_)) => {
                            return Err(LanError::new("send LAN probe", LanErrorKind::Transport));
                        }
                        Wake::Deadline => {
                            return Err(LanError::new("send LAN probe", LanErrorKind::Timeout));
                        }
                        Wake::Cancelled => continue,
                    }
                }
                Err(_) => return Err(LanError::new("send LAN probe", LanErrorKind::Transport)),
            }
        }
    }
    Ok(())
}

async fn collect_candidates(
    sockets: &[(NetworkInterface, Async<UdpSocket>)],
    epoch: NetworkEpoch,
    deadline: Instant,
    require_miio_port: bool,
) -> Result<Vec<LanHelloCandidate>, LanError> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    let mut datagrams_since_yield = 0_u8;
    while Instant::now() < deadline && candidates.len() < MAX_CANDIDATES {
        if datagrams_since_yield >= 32 {
            future::yield_now().await;
            datagrams_since_yield = 0;
        }
        let receives = sockets
            .iter()
            .enumerate()
            .map(|(index, (_, socket))| {
                Box::pin(async move {
                    let mut buffer = [0_u8; 65];
                    let result = socket.recv_from(&mut buffer).await;
                    (index, result, buffer)
                })
            })
            .collect::<Vec<_>>();
        let remaining = deadline.saturating_duration_since(Instant::now());
        let received = future::race(
            async move { Some(select_all(receives).await.0) },
            async move {
                Timer::after(remaining).await;
                None
            },
        )
        .await;
        let Some((index, result, buffer)) = received else {
            break;
        };
        let (length, source) =
            result.map_err(|_| LanError::new("receive LAN probe", LanErrorKind::Transport))?;
        datagrams_since_yield = datagrams_since_yield.saturating_add(1);
        let SocketAddr::V4(source) = source else {
            continue;
        };
        if require_miio_port && source.port() != 54321 {
            continue;
        }
        let Some(hello) = parse_hello(&buffer[..length]) else {
            continue;
        };
        let interface = &sockets[index].0;
        if require_miio_port && !valid_candidate_address(*source.ip(), interface) {
            continue;
        }
        if seen.insert((hello.did, source, interface.index())) {
            candidates.push(LanHelloCandidate {
                did: hello.did,
                address: source,
                interface_index: interface.index(),
                epoch,
                timestamp_hint: hello.timestamp,
                native_hint: hello.subscription_hint.is_some(),
                subscription_hint: hello.subscription_hint,
            });
        }
    }
    Ok(candidates)
}

pub(super) fn parse_hello(packet: &[u8]) -> Option<ParsedHello> {
    if packet.len() != 32 || packet[..4] != [0x21, 0x31, 0, 0x20] {
        return None;
    }
    let did = u64::from_be_bytes(packet[4..12].try_into().ok()?);
    if did == 0 || did == u64::MAX {
        return None;
    }
    let timestamp = u32::from_be_bytes(packet[12..16].try_into().ok()?);
    let subscription_hint = if &packet[16..20] == b"MSUB" && &packet[24..27] == b"PUB" {
        Some(LanSubscriptionHint {
            subscription_timestamp: u32::from_be_bytes(packet[20..24].try_into().ok()?),
            subscription_type: packet[27],
            wildcard_supported: packet[28] == 1,
        })
    } else {
        None
    };
    Some(ParsedHello {
        did,
        timestamp,
        subscription_hint,
    })
}

fn directed_broadcast(interface: &NetworkInterface) -> SocketAddrV4 {
    let address = u32::from(interface.address());
    let netmask = u32::from(interface.netmask());
    SocketAddrV4::new(Ipv4Addr::from(address | !netmask), 54321)
}

fn valid_candidate_address(address: Ipv4Addr, interface: &NetworkInterface) -> bool {
    let raw = u32::from(address);
    let mask = u32::from(interface.netmask());
    let network = u32::from(interface.address()) & mask;
    let broadcast = network | !mask;
    interface.on_link(address)
        && raw != network
        && raw != broadcast
        && address != interface.address()
        && !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && address != Ipv4Addr::BROADCAST
}
