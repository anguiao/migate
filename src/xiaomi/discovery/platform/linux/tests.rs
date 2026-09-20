use super::*;
use crate::xiaomi::discovery::LinkType;

const HEADER: &str = "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n";

fn interface(index: u32, name: &str) -> InterfaceRecord {
    InterfaceRecord {
        index,
        name: name.into(),
        address: Ipv4Addr::new(192, 168, 1, 10),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        up: true,
        point_to_point: false,
        loopback: false,
        link_type: LinkType::Ethernet,
        physical: true,
    }
}

#[test]
fn default_route_uses_the_lowest_metric_and_native_address_byte_order() {
    let gateway = Ipv4Addr::new(192, 168, 1, 1);
    let encoded = u32::from_ne_bytes(gateway.octets());
    let table = format!(
        "{HEADER}wlan0 00000000 {encoded:08X} 0003 0 0 600 00000000 0 0 0\n\
         eth0 00000000 {encoded:08X} 0003 0 0 100 00000000 0 0 0\n"
    );
    assert_eq!(
        parse_default_route(&[interface(2, "eth0"), interface(3, "wlan0")], &table).unwrap(),
        Some(DefaultRoute {
            interface_index: 2,
            gateway: RouteGateway::Ipv4(gateway)
        })
    );
}

#[test]
fn direct_vpn_default_route_is_tracked_even_without_a_physical_link() {
    let record = InterfaceRecord {
        physical: false,
        point_to_point: true,
        ..interface(7, "tun0")
    };
    let table = format!("{HEADER}tun0 00000000 00000000 0001 0 0 0 00000000 0 0 0\n");
    assert_eq!(
        parse_default_route(&[record], &table).unwrap(),
        Some(DefaultRoute {
            interface_index: 7,
            gateway: RouteGateway::Interface
        })
    );
}

#[test]
fn absent_down_rejected_and_non_default_routes_are_not_selected() {
    assert_eq!(parse_default_route(&[], HEADER).unwrap(), None);
    let table = format!(
        "{HEADER}eth0 00000000 00000000 0000 0 0 0 00000000 0 0 0\n\
         eth0 00000000 00000000 0201 0 0 0 00000000 0 0 0\n\
         eth0 00000000 00000000 0001 0 0 0 00000080 0 0 0\n\
         eth0 0000000A 00000000 0001 0 0 0 000000FF 0 0 0\n"
    );
    assert_eq!(
        parse_default_route(&[interface(2, "eth0")], &table).unwrap(),
        None
    );
}

#[test]
fn malformed_route_tables_and_unknown_interfaces_fail_closed() {
    for table in [
        String::new(),
        "unexpected header\n".into(),
        format!("{HEADER}eth0 00000000\n"),
        format!("{HEADER}eth0 00000000 invalid 0003 0 0 0 00000000 0 0 0\n"),
        format!("{HEADER}eth0 00000000 00000000 invalid 0 0 0 00000000 0 0 0\n"),
        format!("{HEADER}eth0 00000000 00000000 0001 0 0 invalid 00000000 0 0 0\n"),
        format!("{HEADER}missing0 00000000 00000000 0001 0 0 0 00000000 0 0 0\n"),
    ] {
        assert!(
            parse_default_route(&[interface(2, "eth0")], &table).is_err(),
            "{table}"
        );
    }
}

#[test]
fn sysfs_metadata_uses_hardware_backing_instead_of_interface_names() {
    let directory = tempfile::tempdir().unwrap();
    let sysfs = directory.path().canonicalize().unwrap();
    std::fs::create_dir_all(sysfs.join("class/net")).unwrap();
    for (name, hardware_path, has_device, expected_physical, raw_type) in [
        (
            "lan-custom",
            "devices/pci0000/ethernet/net/lan-custom",
            true,
            true,
            "1",
        ),
        ("wlan0", "devices/pci0000/wifi/net/wlan0", true, true, "1"),
        ("eth0", "devices/virtual/net/eth0", false, false, "1"),
        ("en0", "devices/virtual/net/en0", true, false, "1"),
        ("lo", "devices/virtual/net/lo", false, false, "772"),
    ] {
        let hardware = sysfs.join(hardware_path);
        std::fs::create_dir_all(&hardware).unwrap();
        std::fs::write(hardware.join("type"), raw_type).unwrap();
        if has_device {
            std::os::unix::fs::symlink(hardware.parent().unwrap(), hardware.join("device"))
                .unwrap();
        }
        std::os::unix::fs::symlink(&hardware, sysfs.join("class/net").join(name)).unwrap();
        let metadata = interface_link(&sysfs, name).unwrap();
        assert_eq!(metadata.physical, expected_physical, "{name}");
        assert_eq!(
            metadata.link_type,
            if raw_type == "1" {
                LinkType::Ethernet
            } else {
                LinkType::Other(772)
            }
        );
    }
    assert!(interface_link(&sysfs, "missing0").is_err());
}
