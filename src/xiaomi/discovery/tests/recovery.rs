use super::*;
use async_io::Timer;
use futures_lite::future;
use mdns_sd::{IfKind, ServiceDaemon};
use std::{net::UdpSocket, time::Duration};

fn name(value: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for label in value.trim_end_matches('.').split('.') {
        bytes.push(label.len() as u8);
        bytes.extend_from_slice(label.as_bytes());
    }
    bytes.push(0);
    bytes
}

/// A long-lived PTR/TXT and short-lived SRV/address, as advertised by the hub.
fn announcement(port: u16) -> Vec<u8> {
    let instance = format!("expiry.{SERVICE_TYPE}");
    let host = "expiry.local.";
    let profile = format!("profile={}", profile(123, 7, 1, true));
    let mut txt = vec![profile.len() as u8];
    txt.extend_from_slice(profile.as_bytes());
    let mut srv = vec![0, 0, 0, 0];
    srv.extend_from_slice(&port.to_be_bytes());
    srv.extend(name(host));
    let mut packet = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
    for (owner, kind, ttl, data) in [
        (SERVICE_TYPE, 12_u16, 4500_u32, name(&instance)),
        (instance.as_str(), 33, 2, srv),
        (instance.as_str(), 16, 4500, txt),
        (host, 1, 2, vec![127, 0, 0, 1]),
    ] {
        packet.extend(name(owner));
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&1_u16.to_be_bytes());
        packet.extend_from_slice(&ttl.to_be_bytes());
        packet.extend_from_slice(&(data.len() as u16).to_be_bytes());
        packet.extend(data);
    }
    packet
}

async fn until(expected: &str, condition: impl Fn() -> bool) {
    future::or(
        async {
            while !condition() {
                Timer::after(Duration::from_millis(10)).await;
            }
        },
        async {
            Timer::after(Duration::from_secs(5)).await;
            panic!("discovery did not reach: {expected}");
        },
    )
    .await;
}

#[test]
fn rediscovery_clears_long_lived_cache_after_address_expiry_and_keeps_driver_running() {
    block_on(async {
        let reserve = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let daemon = ServiceDaemon::new_with_port(port).unwrap();
        daemon.disable_interface(IfKind::All).unwrap();
        daemon.enable_interface(IfKind::LoopbackV4).unwrap();
        let receiver = daemon.browse(SERVICE_TYPE).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            ServiceEvent::SearchStarted(_)
        ));
        let shutdown = daemon.clone();
        let (discovery, driver) = Discovery::from_events(
            receiver,
            Some(daemon.clone()),
            Some(Box::new(move || {
                shutdown.shutdown().unwrap();
            })),
        );
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let read_metrics = || {
            daemon
                .get_metrics()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
        };
        future::or(
            async { panic!("discovery stopped: {:?}", driver.run().await) },
            async {
                socket
                    .send_to(&announcement(8883), (Ipv4Addr::LOCALHOST, port))
                    .unwrap();
                until("initial resolution", || {
                    !discovery.snapshot().gateways.is_empty()
                })
                .await;
                discovery.rediscover("0000000000000007");
                Timer::after(Duration::from_millis(20)).await;
                assert_eq!(discovery.snapshot().gateways.len(), 1);
                assert_eq!(read_metrics()["cached-ptr"], 1);
                until("address expiry", || {
                    discovery.snapshot().gateways.is_empty()
                })
                .await;
                let metrics = read_metrics();
                assert_eq!(metrics["cached-ptr"], 1);
                assert_eq!(metrics["cached-srv"], 0);

                discovery.rediscover("0000000000000007");
                discovery.rediscover("0000000000000007");
                until("fresh browse", || read_metrics()["cached-ptr"] == 0).await;
                socket
                    .send_to(&announcement(9999), (Ipv4Addr::LOCALHOST, port))
                    .unwrap();
                until("resolution after rediscovery", || {
                    !discovery.snapshot().gateways.is_empty()
                })
                .await;
                assert_eq!(
                    discovery.snapshot().route("0000000000000007", None),
                    Some(route("123", "0000000000000007", "127.0.0.1:9999"))
                );
            },
        )
        .await;
    });
}
