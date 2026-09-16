use std::{
    io,
    net::{SocketAddrV4, UdpSocket},
    num::NonZeroU32,
};

use async_io::Async;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::NetworkInterface;

#[derive(Clone)]
pub struct LocalBinder {
    interface: NetworkInterface,
}

impl LocalBinder {
    pub fn new(interface: NetworkInterface) -> Self {
        Self { interface }
    }

    pub fn udp_socket(&self) -> io::Result<Async<UdpSocket>> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_nonblocking(true)?;
        socket.set_broadcast(true)?;
        self.bind_interface(&socket)?;
        socket.bind(&SockAddr::from(SocketAddrV4::new(
            self.interface.address,
            0,
        )))?;
        Async::new(socket.into())
    }

    fn bind_interface(&self, socket: &Socket) -> io::Result<()> {
        let index = NonZeroU32::new(self.interface.index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "interface index is zero")
        })?;
        socket.bind_device_by_index_v4(Some(index))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::net::Ipv4Addr;

    use socket2::SockRef;

    use super::*;

    fn loopback_interface() -> NetworkInterface {
        let name = std::ffi::CString::new("lo0").unwrap();
        // SAFETY: name is a valid null-terminated interface name.
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        NetworkInterface {
            index,
            name: "lo0".into(),
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
}
