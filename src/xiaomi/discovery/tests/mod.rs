mod support;

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::future::{block_on, poll_once};
use mdns_sd::{InterfaceId, ResolvedService, ScopedIp, ScopedIpV4, ServiceEvent, ServiceInfo};

use super::{
    Discovery, DiscoveryError, GatewayRoute, SERVICE_TYPE,
    announcement::{Registry, parse_profile, socket_address},
};

fn profile(did: u64, group: u64, role: u8, mqtt: bool) -> String {
    let mut bytes = [0_u8; 23];
    bytes[1..9].copy_from_slice(&did.to_be_bytes());
    bytes[9..17].copy_from_slice(&group.to_le_bytes());
    bytes[20] = role << 4;
    bytes[22] = if mqtt { 2 } else { 0 };
    STANDARD.encode(bytes)
}

fn resolved(name: &str, profile: &str, addresses: &str, port: u16) -> ResolvedService {
    ServiceInfo::new(
        SERVICE_TYPE,
        name,
        "hub.local.",
        addresses,
        port,
        &[("profile", profile)][..],
    )
    .unwrap()
    .as_resolved_service()
}

fn route(did: &str, group: &str, address: &str) -> GatewayRoute {
    GatewayRoute {
        did: did.into(),
        group_id: group.into(),
        address: address.parse().unwrap(),
    }
}

#[test]
fn profile_uses_miot_byte_order_and_primary_mqtt_flags() {
    let decoded = parse_profile(&profile(123456789, 0x0123456789abcdef, 1, true)).unwrap();
    assert_eq!(decoded.did, "123456789");
    assert_eq!(decoded.group_id, "0123456789abcdef");
    assert!(decoded.is_master());
    assert!(
        !parse_profile(&profile(123, 7, 2, true))
            .unwrap()
            .is_master()
    );
    assert!(
        !parse_profile(&profile(123, 7, 1, false))
            .unwrap()
            .is_master()
    );
    for value in [String::new(), "!!!".into(), STANDARD.encode([0_u8; 22])] {
        assert!(parse_profile(&value).is_err());
    }
}

#[test]
fn persistent_events_replace_same_service_and_clear_old_groups_and_masters() {
    block_on(async {
        let (sender, receiver) = flume::unbounded();
        let (discovery, driver) = Discovery::with_events(receiver);
        let running = driver.run();
        futures_lite::pin!(running);
        let mut changes = discovery.subscribe();
        let original = resolved("same", &profile(123, 7, 1, true), "192.0.2.2", 8883);
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(original.clone())))
            .unwrap();
        assert!(poll_once(running.as_mut()).await.is_none());
        assert_eq!(changes.changed().await.unwrap().revision, 1);
        assert_eq!(
            discovery.snapshot().route("0000000000000007", None),
            Some(route("123", "0000000000000007", "192.0.2.2:8883"))
        );
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(original)))
            .unwrap();
        poll_once(running.as_mut()).await;
        assert!(poll_once(changes.changed()).await.is_none());

        let changed = resolved("same", &profile(456, 8, 1, true), "192.0.2.3", 9999);
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(changed)))
            .unwrap();
        poll_once(running.as_mut()).await;
        let snapshot = changes.changed().await.unwrap();
        assert!(snapshot.route("0000000000000007", None).is_none());
        assert_eq!(
            snapshot.route("0000000000000008", None),
            Some(route("456", "0000000000000008", "192.0.2.3:9999"))
        );

        for (role, mqtt) in [(2, true), (1, false)] {
            sender
                .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                    "same",
                    &profile(456, 8, role, mqtt),
                    "192.0.2.3",
                    9999,
                ))))
                .unwrap();
            poll_once(running.as_mut()).await;
            assert!(discovery.snapshot().gateways.is_empty());
            sender
                .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                    "same",
                    &profile(456, 8, 1, true),
                    "192.0.2.3",
                    9999,
                ))))
                .unwrap();
            poll_once(running.as_mut()).await;
        }
        sender
            .send(ServiceEvent::ServiceRemoved(
                SERVICE_TYPE.into(),
                format!("same.{SERVICE_TYPE}"),
            ))
            .unwrap();
        poll_once(running.as_mut()).await;
        assert!(discovery.snapshot().gateways.is_empty());
    });
}

#[test]
fn duplicate_interfaces_and_announcements_are_one_gateway_and_address() {
    block_on(async {
        let (sender, receiver) = flume::unbounded();
        let (discovery, driver) = Discovery::with_events(receiver);
        let running = driver.run();
        futures_lite::pin!(running);
        let mut announcement = resolved("same", &profile(123, 7, 1, true), "192.0.2.2", 8883);
        let address = "192.0.2.2".parse::<Ipv4Addr>().unwrap();
        announcement.addresses = [
            ScopedIp::V4(ScopedIpV4::new(
                address,
                InterfaceId {
                    name: "en0".into(),
                    index: 4,
                },
            )),
            ScopedIp::V4(ScopedIpV4::new(
                address,
                InterfaceId {
                    name: "en1".into(),
                    index: 5,
                },
            )),
        ]
        .into_iter()
        .collect();
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(announcement)))
            .unwrap();
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                "same-on-another-interface",
                &profile(123, 7, 1, true),
                "192.0.2.2",
                8883,
            ))))
            .unwrap();
        poll_once(running.as_mut()).await;
        let snapshot = discovery.snapshot();
        assert_eq!(snapshot.gateways.len(), 1);
        assert_eq!(
            snapshot.gateways[0].addresses,
            ["192.0.2.2:8883".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(snapshot.revision, 1);
    });
}

#[test]
fn ipv6_addresses_keep_the_interface_scope_and_reject_unscoped_link_local() {
    use if_addrs::{IfAddr, IfOperStatus, Ifv6Addr, Interface};
    let ip = "fe80::1234".parse::<Ipv6Addr>().unwrap();
    let interface = Interface {
        name: "en0".into(),
        addr: IfAddr::V6(Ifv6Addr {
            ip,
            netmask: Ipv6Addr::UNSPECIFIED,
            prefixlen: 64,
            broadcast: None,
        }),
        index: Some(4),
        oper_status: IfOperStatus::Up,
        is_p2p: false,
    };
    let scoped = ScopedIp::from(&interface);
    assert_eq!(
        socket_address(&scoped, 8883),
        Some(SocketAddr::V6(SocketAddrV6::new(ip, 8883, 0, 4)))
    );
    assert!(socket_address(&ScopedIp::from(std::net::IpAddr::V6(ip)), 8883).is_none());
    let mut announcement = resolved("scoped", &profile(123, 7, 1, true), "192.0.2.2", 8883);
    announcement.addresses = [scoped].into_iter().collect();
    let mut registry = Registry::default();
    registry.resolve(announcement).unwrap();
    assert_eq!(
        registry
            .snapshot()
            .route("0000000000000007", None)
            .unwrap()
            .address,
        SocketAddr::V6(SocketAddrV6::new(ip, 8883, 0, 4))
    );
}

#[test]
fn routes_keep_valid_current_address_and_change_when_master_or_address_disappears() {
    let mut registry = Registry::default();
    registry
        .resolve(resolved(
            "main",
            &profile(456, 7, 1, true),
            "192.0.2.3,192.0.2.2",
            8883,
        ))
        .unwrap();
    let snapshot = registry.snapshot();
    let current = snapshot.route("0000000000000007", None).unwrap();
    assert_eq!(current, route("456", "0000000000000007", "192.0.2.2:8883"));
    registry
        .resolve(resolved(
            "backup",
            &profile(123, 7, 1, true),
            "192.0.2.1",
            8883,
        ))
        .unwrap();
    assert_eq!(
        registry
            .snapshot()
            .route("0000000000000007", Some(&current)),
        Some(current.clone())
    );
    registry
        .resolve(resolved(
            "main",
            &profile(456, 7, 1, true),
            "192.0.2.3",
            8883,
        ))
        .unwrap();
    let updated = registry
        .snapshot()
        .route("0000000000000007", Some(&current))
        .unwrap();
    assert_eq!(updated, route("456", "0000000000000007", "192.0.2.3:8883"));
    registry
        .resolve(resolved(
            "main",
            &profile(456, 7, 2, true),
            "192.0.2.3",
            8883,
        ))
        .unwrap();
    assert_eq!(
        registry
            .snapshot()
            .route("0000000000000007", Some(&updated)),
        Some(route("123", "0000000000000007", "192.0.2.1:8883"))
    );
    assert!(
        registry
            .snapshot()
            .route("remote", Some(&updated))
            .is_none()
    );
}

#[test]
fn connection_candidates_include_all_master_addresses_only_in_the_current_group() {
    let mut registry = Registry::default();
    for (name, did, group, role, mqtt, addresses) in [
        ("current", 456, 7, 1, true, "192.0.2.3,192.0.2.2"),
        ("next", 123, 7, 1, true, "192.0.2.1"),
        ("foreign", 1, 8, 1, true, "192.0.2.4"),
        ("backup", 2, 7, 2, true, "192.0.2.5"),
        ("no-mqtt", 3, 7, 1, false, "192.0.2.6"),
    ] {
        registry
            .resolve(resolved(
                name,
                &profile(did, group, role, mqtt),
                addresses,
                8883,
            ))
            .unwrap();
    }
    let current = route("456", "0000000000000007", "192.0.2.3:8883");
    assert_eq!(
        registry
            .snapshot()
            .routes(&current.group_id, Some(&current)),
        [
            current.clone(),
            route("456", "0000000000000007", "192.0.2.2:8883"),
            route("123", "0000000000000007", "192.0.2.1:8883"),
        ]
    );
    assert!(
        registry
            .snapshot()
            .routes("remote", Some(&current))
            .is_empty()
    );
}

#[test]
fn invalid_replacement_removes_the_previous_route_without_stopping_discovery() {
    block_on(async {
        let (sender, receiver) = flume::unbounded();
        let (discovery, driver) = Discovery::with_events(receiver);
        let running = driver.run();
        futures_lite::pin!(running);
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                "same",
                &profile(123, 7, 1, true),
                "192.0.2.2",
                8883,
            ))))
            .unwrap();
        poll_once(running.as_mut()).await;
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                "same",
                "invalid",
                "192.0.2.2",
                8883,
            ))))
            .unwrap();
        assert!(poll_once(running.as_mut()).await.is_none());
        assert!(discovery.snapshot().gateways.is_empty());
        sender
            .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                "new",
                &profile(123, 7, 1, true),
                "192.0.2.2",
                8883,
            ))))
            .unwrap();
        poll_once(running.as_mut()).await;
        assert_eq!(discovery.snapshot().gateways.len(), 1);
    });
}

#[test]
fn dropping_driver_clears_addresses_wakes_consumers_and_drops_the_event_source() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    block_on(async {
        let (sender, receiver) = flume::unbounded();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let observed = shutdowns.clone();
        let (discovery, driver) = Discovery::from_events(
            receiver,
            Some(Box::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            })),
        );
        let mut changes = discovery.subscribe();
        {
            let running = driver.run();
            futures_lite::pin!(running);
            sender
                .send(ServiceEvent::ServiceResolved(Box::new(resolved(
                    "same",
                    &profile(123, 7, 1, true),
                    "192.0.2.2",
                    8883,
                ))))
                .unwrap();
            poll_once(running.as_mut()).await;
            assert_eq!(changes.changed().await.unwrap().gateways.len(), 1);
            assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
        }
        assert!(discovery.snapshot().gateways.is_empty());
        assert!(matches!(
            changes.changed().await,
            Err(DiscoveryError::Stopped)
        ));
        assert!(sender.is_disconnected());
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn source_shutdown_stops_the_driver_and_invokes_cleanup_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    block_on(async {
        for search_stopped in [false, true] {
            let (sender, receiver) = flume::unbounded();
            let shutdowns = Arc::new(AtomicUsize::new(0));
            let observed = shutdowns.clone();
            let (discovery, driver) = Discovery::from_events(
                receiver,
                Some(Box::new(move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                })),
            );
            if search_stopped {
                sender
                    .send(ServiceEvent::SearchStopped(SERVICE_TYPE.into()))
                    .unwrap();
            }
            drop(sender);
            assert_eq!(driver.run().await, Err(DiscoveryError::Stopped));
            assert!(discovery.snapshot().gateways.is_empty());
            assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        }
    });
}
