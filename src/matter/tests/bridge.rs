use super::Advertisement;
use std::net::UdpSocket;

#[test]
fn rebuilt_sockets_keep_the_advertised_dynamic_port_and_release_it_on_drop() {
    let mut advertisement = Advertisement::new(0);
    let first = advertisement.bind().unwrap();
    let address = first.get_ref().local_addr().unwrap();
    assert_ne!(address.port(), 0);
    assert!(UdpSocket::bind(address).is_err());
    drop(first);

    let second = advertisement.bind().unwrap();
    assert_eq!(second.get_ref().local_addr().unwrap(), address);
    drop(second);
    // Retaining advertisements does not retain the previous instance's socket.
    let released = UdpSocket::bind(address).unwrap();
    drop(released);
    let third = advertisement.bind().unwrap();
    assert_eq!(third.get_ref().local_addr().unwrap(), address);
}
