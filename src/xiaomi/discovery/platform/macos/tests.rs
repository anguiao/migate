use super::*;
use std::net::Ipv4Addr;

#[test]
fn direct_interface_default_route_does_not_require_a_gateway_field() {
    let records = vec![InterfaceRecord {
        index: 7,
        name: "utun4".into(),
        address: Ipv4Addr::new(10, 0, 0, 2),
        netmask: Ipv4Addr::new(255, 0, 0, 0),
        up: true,
        point_to_point: true,
        loopback: false,
        link_type: LinkType::Other(0),
        physical: false,
    }];
    let route = parse_default_route(
        &records,
        "destination: default\nmask: default\ninterface: utun4\nflags: <UP,DONE>\n",
    )
    .unwrap();
    assert_eq!(route.interface_index, 7);
    assert_eq!(route.gateway, RouteGateway::Interface);
}
