use super::*;
use std::thread;

#[test]
fn finite_discovery_collects_address_clues_only() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let target = match socket.local_addr().unwrap() {
        SocketAddr::V4(target) => target,
        _ => unreachable!(),
    };
    let peer = thread::spawn(move || {
        let mut buffer = [0; 32];
        let (_, source) = socket.recv_from(&mut buffer).unwrap();
        let mut reply = wire::HELLO;
        reply[4..12].copy_from_slice(&656878122_u64.to_be_bytes());
        reply[12..16].copy_from_slice(&42_u32.to_be_bytes());
        socket.send_to(&reply, source).unwrap();
    });
    let found = future::block_on(scan(&[target], Duration::from_millis(50)));
    assert_eq!(found.get(&656878122), Some(&Ipv4Addr::LOCALHOST));
    peer.join().unwrap();
}

#[test]
fn simultaneous_recovery_requests_share_one_scan() {
    use crate::xiaomi::lan::{Token, tests::support::FakeDevice};
    use std::sync::atomic::Ordering;
    let token = Token::parse("00112233445566778899aabbccddeeff").unwrap();
    let peer = FakeDevice::new(token.clone(), token);
    let discovery = Discovery::default();
    let (found, absent) = future::block_on(future::zip(
        discovery.address_at(656878122, peer.address, Duration::from_millis(30)),
        discovery.address_at(42, peer.address, Duration::from_millis(30)),
    ));
    assert_eq!(found, Some(Ipv4Addr::LOCALHOST));
    assert_eq!(absent, None);
    assert_eq!(peer.packets.load(Ordering::Relaxed), 1);
}

impl Discovery {
    pub(in crate::xiaomi) async fn address_at(
        &self,
        did: u64,
        target: SocketAddr,
        timeout: Duration,
    ) -> Option<Ipv4Addr> {
        let SocketAddr::V4(target) = target else {
            panic!("IPv4 required")
        };
        self.scan_for(did, || vec![target], timeout).await
    }
}
