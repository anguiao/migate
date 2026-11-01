use super::wire;
use async_io::{Async, Timer};
use async_lock::Mutex;
use futures_lite::future;
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    rc::Rc,
    time::{Duration, Instant},
};

#[cfg(not(test))]
use std::collections::BTreeSet;

const PORT: u16 = 54321;
const WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct Discovery {
    cached: Rc<Mutex<Option<Scan>>>,
    #[cfg(test)]
    target: Rc<std::cell::Cell<(SocketAddrV4, Duration)>>,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            cached: Rc::new(Mutex::new(None)),
            #[cfg(test)]
            target: Rc::new(std::cell::Cell::new((
                SocketAddrV4::new(Ipv4Addr::BROADCAST, PORT),
                WINDOW,
            ))),
        }
    }
}

struct Scan {
    completed: Instant,
    devices: BTreeMap<u64, Ipv4Addr>,
}

impl Discovery {
    #[cfg(test)]
    pub(crate) fn configure_test(&self, target: SocketAddr, timeout: Duration) {
        let SocketAddr::V4(target) = target else {
            panic!("IPv4 required")
        };
        self.target.set((target, timeout));
    }
    /// Concurrent recovery attempts share a single bounded broadcast scan.
    pub(crate) async fn address(&self, did: u64) -> Option<Ipv4Addr> {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_none_or(|scan| scan.completed.elapsed() >= Duration::from_secs(1))
        {
            #[cfg(test)]
            let (target, timeout) = self.target.get();
            #[cfg(test)]
            let targets = vec![target];
            #[cfg(not(test))]
            let (targets, timeout) = (broadcast_targets(), WINDOW);
            let devices = scan(&targets, timeout).await;
            *cached = Some(Scan {
                completed: Instant::now(),
                devices,
            });
        }
        cached.as_ref().unwrap().devices.get(&did).copied()
    }
}

#[cfg(not(test))]
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
#[path = "tests/discovery.rs"]
mod tests;
