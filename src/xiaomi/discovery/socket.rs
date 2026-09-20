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

#[cfg(test)]
mod tests;
