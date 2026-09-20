use std::net::Ipv4Addr;

use futures_lite::future::block_on;

use super::*;
use crate::xiaomi::discovery::{DefaultRoute, InterfaceRecord, LinkType, RouteGateway};

#[test]
fn isolated_browser_can_refresh_and_stop() {
    let network = NetworkSnapshot::select(
        vec![InterfaceRecord {
            index: 999_999,
            name: "en-test".into(),
            address: Ipv4Addr::new(192, 0, 2, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            up: true,
            point_to_point: false,
            loopback: false,
            link_type: LinkType::Ethernet,
            physical: true,
        }],
        Some(DefaultRoute {
            interface_index: 999_999,
            gateway: RouteGateway::Ipv4(Ipv4Addr::new(192, 0, 2, 254)),
        }),
    )
    .unwrap();
    let mut browser =
        DiscoveryBrowser::start_with_daemon(&network, ServiceDaemon::new_with_port(0)).unwrap();
    browser.refresh().unwrap();
    block_on(browser.stop()).unwrap();
}
