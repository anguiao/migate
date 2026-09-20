use super::*;
use crate::{storage::DeviceToken, xiaomi::catalog::compile_spec};

fn interface() -> NetworkInterface {
    NetworkInterface {
        index: 4,
        name: "en-test".into(),
        address: Ipv4Addr::new(192, 168, 8, 10),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        prefix_len: 24,
    }
}

#[test]
fn target_requires_on_link_unicast_distinct_from_local_host() {
    let interface = interface();
    assert!(validate_unicast_target(Ipv4Addr::new(192, 168, 8, 20), &interface).is_ok());
    for invalid in [
        Ipv4Addr::new(192, 168, 8, 0),
        Ipv4Addr::new(192, 168, 8, 255),
        Ipv4Addr::new(192, 168, 8, 10),
        Ipv4Addr::new(192, 168, 9, 20),
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(224, 0, 0, 1),
        Ipv4Addr::LOCALHOST,
    ] {
        assert!(
            validate_unicast_target(invalid, &interface).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn catalog_light_group_is_never_a_credentialed_lan_target() {
    let compiled = compile_spec(
        "mijia.light.group3",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/miot_specs/mijia.light.group3.json"
        )),
    )
    .unwrap();
    let device = CatalogDevice {
        home_id: "home".into(),
        room_id: None,
        parent_did: "42".into(),
        name: "group".into(),
        model: "mijia.light.group3".into(),
        spec_type: Some(compiled.type_urn),
        pid: Some(0),
        token: Some(DeviceToken(vec![1; 16])),
        online: Some(true),
        local_ip: Some("192.168.8.20".into()),
        parent_id: None,
        features: compiled.features,
    };
    assert!(LanTarget::from_catalog(&device, "home", interface(), NetworkEpoch::new(1)).is_err());
}

#[test]
fn authenticated_target_can_start_from_matching_hello_when_cached_ip_is_missing() {
    let compiled = compile_spec(
        "mijia.light.group3",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/miot_specs/mijia.light.group3.json"
        )),
    )
    .unwrap();
    let device = CatalogDevice {
        home_id: "home".into(),
        room_id: None,
        parent_did: "42".into(),
        name: "lamp".into(),
        model: "yeelink.light.color2".into(),
        spec_type: Some(compiled.type_urn),
        pid: Some(0),
        token: Some(DeviceToken(vec![1; 16])),
        online: None,
        local_ip: None,
        parent_id: None,
        features: compiled.features,
    };
    let candidate = LanHelloCandidate {
        did: 42,
        address: SocketAddrV4::new(Ipv4Addr::new(192, 168, 8, 20), 54321),
        interface_index: 4,
        epoch: NetworkEpoch::new(2),
        timestamp_hint: 901,
        native_hint: true,
        subscription_hint: None,
    };
    let target = LanTarget::from_candidate(
        &device,
        "home",
        interface(),
        NetworkEpoch::new(2),
        &candidate,
    )
    .unwrap();
    assert_eq!(target.address(), candidate.address);
    assert_eq!(target.timestamp_hint(), Some(901));
}
