use std::net::Ipv4Addr;

use socket2::SockRef;

use super::*;

fn loopback_interface() -> NetworkInterface {
    let interface = if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .find(|interface| interface.ip() == Ipv4Addr::LOCALHOST)
        .expect("IPv4 loopback interface");
    let index = interface.index.unwrap();
    NetworkInterface {
        index,
        name: interface.name,
        address: Ipv4Addr::LOCALHOST,
        netmask: Ipv4Addr::new(255, 0, 0, 0),
        prefix_len: 8,
    }
}

#[test]
fn udp_socket_is_bound_to_the_selected_source_and_interface() {
    let binder = LocalBinder::new(loopback_interface());
    let udp = binder.udp_socket().unwrap();
    assert_eq!(
        udp.get_ref().local_addr().unwrap().ip(),
        Ipv4Addr::LOCALHOST
    );
    assert!(udp.get_ref().broadcast().unwrap());
    assert_eq!(
        SockRef::from(udp.get_ref()).device_index_v4().unwrap(),
        NonZeroU32::new(binder.interface.index)
    );
}
