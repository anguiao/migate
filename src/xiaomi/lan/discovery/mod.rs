use super::{PORT, wire};
use async_io::{Async, Timer};
use async_lock::Mutex;
use futures_lite::future;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    rc::Rc,
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
pub(in crate::xiaomi) struct Discovery {
    cached: Rc<Mutex<Option<Scan>>>,
}

struct Scan {
    completed: Instant,
    devices: BTreeMap<u64, Ipv4Addr>,
}

impl Discovery {
    /// Concurrent recovery attempts share a single bounded broadcast scan.
    pub(in crate::xiaomi) async fn address(&self, did: u64) -> Option<Ipv4Addr> {
        self.scan_for(did, broadcast_targets, WINDOW).await
    }

    async fn scan_for(
        &self,
        did: u64,
        targets: impl FnOnce() -> Vec<SocketAddrV4>,
        timeout: Duration,
    ) -> Option<Ipv4Addr> {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_none_or(|scan| scan.completed.elapsed() >= Duration::from_secs(1))
        {
            let devices = scan(&targets(), timeout).await;
            *cached = Some(Scan {
                completed: Instant::now(),
                devices,
            });
        }
        cached.as_ref().unwrap().devices.get(&did).copied()
    }
}

fn broadcast_targets() -> Vec<SocketAddrV4> {
    let mut targets = BTreeSet::from([SocketAddrV4::new(Ipv4Addr::BROADCAST, PORT)]);
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() || !interface.is_oper_up() || interface.is_p2p() {
                continue;
            }
            if let if_addrs::IfAddr::V4(address) = interface.addr
                && let Some(broadcast) = address.broadcast
            {
                targets.insert(SocketAddrV4::new(broadcast, PORT));
            }
        }
    }
    targets.into_iter().collect()
}

async fn scan(targets: &[SocketAddrV4], timeout: Duration) -> BTreeMap<u64, Ipv4Addr> {
    let mut found = BTreeMap::new();
    let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) else {
        return found;
    };
    if socket.set_broadcast(true).is_err() {
        return found;
    }
    let Ok(socket) = Async::new(socket) else {
        return found;
    };
    future::or(
        async {
            for target in targets {
                let _ = socket.send_to(&wire::HELLO, *target).await;
            }
            let mut buffer = [0; 2048];
            while let Ok((length, SocketAddr::V4(peer))) = socket.recv_from(&mut buffer).await {
                if targets.iter().any(|target| target.port() == peer.port())
                    && let Some((did, _)) = wire::hello(&buffer[..length])
                {
                    found.insert(did, *peer.ip());
                }
            }
        },
        async {
            Timer::after(timeout).await;
        },
    )
    .await;
    found
}

#[cfg(test)]
mod tests;
