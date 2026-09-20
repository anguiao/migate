use base64::{Engine as _, engine::general_purpose::STANDARD};
use migate::xiaomi::discovery::{
    DefaultRoute, DiscoveryRegistry, GatewayProfile, InterfaceRecord, LinkType, MdnsEvent,
    NetworkEpoch, NetworkSnapshot, NetworkTracker, RawResolvedService, RouteGateway, ScopedIpv4,
    TimeSample, WakeSample,
};
use std::net::Ipv4Addr;
use std::time::Duration;

fn profile(did: u64, group: [u8; 8], master: bool, mqtt: bool) -> String {
    let mut bytes = [0u8; 23];
    bytes[1..9].copy_from_slice(&did.to_be_bytes());
    bytes[9..17].copy_from_slice(&group);
    bytes[20] = if master { 0x10 } else { 0x20 };
    bytes[22] = if mqtt { 0x02 } else { 0 };
    STANDARD.encode(bytes)
}

fn interface(index: u32, name: &str, address: [u8; 4], prefix: u8) -> InterfaceRecord {
    InterfaceRecord {
        index,
        name: name.into(),
        address: Ipv4Addr::from(address),
        netmask: Ipv4Addr::from(u32::MAX.checked_shl((32 - prefix) as u32).unwrap_or(0)),
        up: true,
        point_to_point: false,
        loopback: false,
        link_type: LinkType::Ethernet,
        physical: true,
    }
}

fn network() -> NetworkSnapshot {
    NetworkSnapshot::select(
        vec![interface(4, "en0", [192, 168, 8, 10], 24)],
        Some(DefaultRoute {
            interface_index: 4,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 168, 8, 1)),
        }),
    )
    .unwrap()
}

fn resolved(
    instance: &str,
    did: u64,
    group: [u8; 8],
    master: bool,
    address: Ipv4Addr,
    interface_index: u32,
) -> MdnsEvent {
    MdnsEvent::Resolved(RawResolvedService {
        service_type: "_miot-central._tcp.local.".into(),
        instance: instance.into(),
        port: 8883,
        profile: profile(did, group, master, true),
        addresses: vec![ScopedIpv4 {
            address,
            interface_indexes: vec![interface_index],
        }],
    })
}

#[test]
fn profile_parses_wire_fields_and_rejects_invalid_inputs() {
    let parsed = GatewayProfile::parse_base64(&profile(
        0x0102_0304_0506_0708,
        [1, 2, 3, 4, 5, 6, 7, 8],
        true,
        true,
    ))
    .unwrap();
    assert_eq!(parsed.gateway_did, 0x0102_0304_0506_0708);
    assert_eq!(parsed.home_group, "0807060504030201");
    assert!(parsed.master);
    assert!(parsed.mqtt);

    for invalid in [
        "not base64".into(),
        STANDARD.encode([0u8; 22]),
        profile(0, [0; 8], true, true),
    ] {
        assert!(GatewayProfile::parse_base64(&invalid).is_err());
    }
}

#[test]
fn physical_interface_selection_and_on_link_checks_do_not_trust_private_addresses() {
    let virtual_interface = |index, name, address, prefix| InterfaceRecord {
        physical: false,
        ..interface(index, name, address, prefix)
    };
    let records = vec![
        interface(4, "en0", [192, 168, 8, 10], 24),
        virtual_interface(5, "utun3", [10, 0, 0, 2], 8),
        virtual_interface(6, "bridge0", [172, 16, 0, 2], 16),
        virtual_interface(9, "vmnet8", [192, 168, 64, 1], 24),
        virtual_interface(10, "vmenet0", [192, 168, 65, 1], 24),
        virtual_interface(11, "feth0", [192, 168, 66, 1], 24),
        virtual_interface(12, "eth0", [172, 17, 0, 2], 16),
        InterfaceRecord {
            point_to_point: true,
            ..interface(7, "en7", [192, 168, 9, 2], 24)
        },
        InterfaceRecord {
            up: false,
            ..interface(8, "en8", [192, 168, 10, 2], 24)
        },
    ];
    let snapshot = NetworkSnapshot::select(
        records,
        Some(DefaultRoute {
            interface_index: 4,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 168, 8, 1)),
        }),
    )
    .unwrap();
    assert_eq!(snapshot.interfaces().len(), 1);
    assert!(
        snapshot
            .interface(4)
            .unwrap()
            .on_link(Ipv4Addr::new(192, 168, 8, 99))
    );
    assert!(
        !snapshot
            .interface(4)
            .unwrap()
            .on_link(Ipv4Addr::new(192, 168, 9, 99))
    );
    assert!(
        NetworkSnapshot::select(
            vec![virtual_interface(5, "utun3", [10, 0, 0, 2], 8)],
            Some(DefaultRoute {
                interface_index: 5,
                gateway: RouteGateway::Ipv4(Ipv4Addr::new(10, 0, 0, 1))
            }),
        )
        .is_err()
    );
    let vpn_route = NetworkSnapshot::select(
        vec![
            interface(4, "en0", [192, 168, 8, 10], 24),
            virtual_interface(5, "utun3", [10, 0, 0, 2], 8),
        ],
        Some(DefaultRoute {
            interface_index: 5,
            gateway: RouteGateway::Link(5),
        }),
    )
    .unwrap();
    assert_eq!(vpn_route.interfaces().len(), 1);
    assert!(
        NetworkSnapshot::select(vec![interface(4, "en0", [192, 168, 8, 10], 24)], None,).is_ok()
    );
}

#[test]
fn physical_interface_selection_accepts_system_names_without_a_prefix_allowlist() {
    for name in ["en0", "eth0", "enp3s0", "wlan0", "wlp2s0", "lan-custom"] {
        let snapshot =
            NetworkSnapshot::select(vec![interface(2, name, [192, 168, 1, 10], 24)], None).unwrap();
        assert_eq!(snapshot.interfaces()[0].name(), name);
    }
}

#[test]
fn epoch_changes_for_route_interface_prefix_and_sleep_and_does_not_reuse_old_snapshot() {
    let mut tracker = NetworkTracker::new();
    let first = tracker.observe(
        network(),
        TimeSample {
            monotonic_millis: 100,
            wall_unix_millis: 1_000,
        },
    );
    let same = tracker.observe(
        network(),
        TimeSample {
            monotonic_millis: 200,
            wall_unix_millis: 1_100,
        },
    );
    assert_eq!(same.epoch, first.epoch);

    let changed = NetworkSnapshot::select(
        vec![interface(4, "en0", [192, 168, 8, 10], 25)],
        Some(DefaultRoute {
            interface_index: 4,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 168, 8, 1)),
        }),
    )
    .unwrap();
    let prefix = tracker.observe(
        changed,
        TimeSample {
            monotonic_millis: 300,
            wall_unix_millis: 1_200,
        },
    );
    assert!(prefix.epoch > same.epoch);
    let sleep = tracker.observe(
        network(),
        TimeSample {
            monotonic_millis: 400,
            wall_unix_millis: 20_000,
        },
    );
    assert!(sleep.epoch > prefix.epoch);

    let route = NetworkSnapshot::select(
        vec![interface(4, "en0", [192, 168, 8, 10], 24)],
        Some(DefaultRoute {
            interface_index: 4,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 168, 8, 2)),
        }),
    )
    .unwrap();
    assert!(
        tracker
            .observe(
                route.clone(),
                TimeSample {
                    monotonic_millis: 500,
                    wall_unix_millis: 20_100
                }
            )
            .epoch
            > sleep.epoch
    );
    let invalidated = tracker.invalidate();
    assert!(invalidated > sleep.epoch);
    let restored = tracker.observe(
        route,
        TimeSample {
            monotonic_millis: 600,
            wall_unix_millis: 20_200,
        },
    );
    assert_eq!(restored.epoch, invalidated);
    let short_sleep = tracker.observe_with_wake(
        network(),
        TimeSample {
            monotonic_millis: 700,
            wall_unix_millis: 20_300,
        },
        true,
    );
    assert!(short_sleep.epoch > restored.epoch);
    let clock_sleep = tracker.observe_with_wake_sample(
        network(),
        TimeSample {
            monotonic_millis: 800,
            wall_unix_millis: 20_400,
        },
        WakeSample {
            active_millis: 1_000,
            continuous_millis: 1_000,
        },
    );
    let clock_resumed = tracker.observe_with_wake_sample(
        network(),
        TimeSample {
            monotonic_millis: 900,
            wall_unix_millis: 20_500,
        },
        WakeSample {
            active_millis: 1_100,
            continuous_millis: 1_300,
        },
    );
    assert!(clock_resumed.epoch > clock_sleep.epoch);
}

#[test]
fn registry_filters_scoped_addresses_and_handles_dedup_remove_and_master_switch() {
    let mut registry = DiscoveryRegistry::new(NetworkEpoch::new(1), network());
    let group_a = [1; 8];
    let group_b = [2; 8];
    registry
        .apply(resolved(
            "hub-a-1",
            10,
            group_a,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            4,
        ))
        .unwrap();
    registry
        .apply(resolved(
            "hub-a-2",
            10,
            group_a,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            4,
        ))
        .unwrap();
    registry
        .apply(resolved(
            "hub-a-backup",
            11,
            group_a,
            false,
            Ipv4Addr::new(192, 168, 8, 21),
            4,
        ))
        .unwrap();
    registry
        .apply(resolved(
            "hub-b",
            20,
            group_b,
            true,
            Ipv4Addr::new(192, 168, 8, 22),
            4,
        ))
        .unwrap();
    assert_eq!(registry.candidates().len(), 2);

    registry
        .apply(MdnsEvent::Removed {
            service_type: "_miot-central._tcp.local.".into(),
            instance: "hub-a-1".into(),
        })
        .unwrap();
    assert_eq!(registry.candidates().len(), 2);
    registry
        .apply(MdnsEvent::Removed {
            service_type: "_miot-central._tcp.local.".into(),
            instance: "hub-a-2".into(),
        })
        .unwrap();
    assert_eq!(registry.candidates().len(), 1);

    registry
        .apply(resolved(
            "hub-a-backup",
            11,
            group_a,
            true,
            Ipv4Addr::new(192, 168, 8, 21),
            4,
        ))
        .unwrap();
    assert_eq!(registry.candidates().len(), 2);
    assert!(
        registry
            .candidates()
            .iter()
            .all(|candidate| candidate.unverified)
    );
}

#[test]
fn registry_rejects_wrong_type_port_unscoped_and_off_link_events_and_clears_on_epoch() {
    let mut registry = DiscoveryRegistry::new(NetworkEpoch::new(1), network());
    let group = [3; 8];
    let mut cases = vec![
        resolved(
            "wrong-type",
            1,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            4,
        ),
        resolved(
            "zero-port",
            2,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            4,
        ),
        resolved(
            "wrong-interface",
            3,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            99,
        ),
        resolved("off-link", 4, group, true, Ipv4Addr::new(10, 2, 3, 4), 4),
    ];
    if let MdnsEvent::Resolved(value) = &mut cases[0] {
        value.service_type = "_http._tcp.local.".into();
    }
    if let MdnsEvent::Resolved(value) = &mut cases[1] {
        value.port = 0;
    }
    for event in cases {
        assert!(registry.apply(event).is_err());
    }
    registry
        .apply(resolved(
            "good",
            5,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 20),
            4,
        ))
        .unwrap();
    assert_eq!(registry.candidates().len(), 1);
    registry.update_network(NetworkEpoch::new(99), network());
    assert!(registry.candidates().is_empty());
}

#[test]
fn latest_profile_controls_duplicate_role_and_rejects_home_group_conflicts() {
    let mut registry = DiscoveryRegistry::new(NetworkEpoch::new(1), network());
    let group = [4; 8];
    registry
        .apply(resolved(
            "hub-copy-a",
            30,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 30),
            4,
        ))
        .unwrap();
    registry
        .apply(resolved(
            "hub-copy-b",
            30,
            group,
            true,
            Ipv4Addr::new(192, 168, 8, 31),
            4,
        ))
        .unwrap();
    assert_eq!(registry.candidates().len(), 1);
    registry
        .apply(resolved(
            "hub-copy-b",
            30,
            group,
            false,
            Ipv4Addr::new(192, 168, 8, 31),
            4,
        ))
        .unwrap();
    assert!(registry.candidates().is_empty());
    registry
        .apply(MdnsEvent::Removed {
            service_type: "_miot-central._tcp.local.".into(),
            instance: "hub-copy-b".into(),
        })
        .unwrap();
    assert!(registry.candidates().is_empty());
    assert!(
        registry
            .apply(resolved(
                "conflict",
                30,
                [5; 8],
                true,
                Ipv4Addr::new(192, 168, 8, 32),
                4,
            ))
            .is_err()
    );
}

#[test]
fn scoped_endpoint_selects_the_matching_address_on_a_multi_address_interface() {
    let snapshot = NetworkSnapshot::select(
        vec![
            interface(4, "en0", [192, 168, 8, 10], 24),
            interface(4, "en0", [10, 20, 30, 10], 24),
        ],
        Some(DefaultRoute {
            interface_index: 4,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 168, 8, 1)),
        }),
    )
    .unwrap();
    let mut registry = DiscoveryRegistry::new(NetworkEpoch::new(1), snapshot);
    registry
        .apply(resolved(
            "multi-address",
            40,
            [6; 8],
            true,
            Ipv4Addr::new(10, 20, 30, 20),
            4,
        ))
        .unwrap();
    assert_eq!(
        registry.candidates()[0].endpoints[0].source_address,
        Ipv4Addr::new(10, 20, 30, 10)
    );
}

#[test]
fn native_network_collection_runs_and_reports_a_safe_result() {
    let wake = migate::xiaomi::discovery::capture_wake_sample().unwrap();
    assert!(wake.continuous_millis >= wake.active_millis);
    match futures_lite::future::block_on(migate::xiaomi::discovery::collect_network_snapshot(
        Duration::from_secs(1),
    )) {
        Ok(snapshot) => {
            assert!(!snapshot.interfaces().is_empty());
            assert!(
                snapshot
                    .interfaces()
                    .iter()
                    .all(|interface| interface.index() > 0)
            );
        }
        Err(error) => assert!(!error.to_string().is_empty()),
    }
}
