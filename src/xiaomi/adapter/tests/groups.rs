use super::*;
use std::{
    cell::Cell,
    io,
    task::{Context as TaskContext, Poll},
};

use super::support::group_record;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    ))
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

fn runtime_socket_live(port: u16) -> bool {
    std::net::UdpSocket::bind(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::UNSPECIFIED,
        port,
        0,
        0,
    ))
    .is_err()
}

struct TerminalOutput {
    bytes: Vec<u8>,
    replies: flume::Sender<Vec<u8>>,
}

impl futures_lite::io::AsyncWrite for TerminalOutput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let bytes = std::mem::take(&mut self.bytes);
        self.replies.send(bytes).unwrap();
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

async fn terminal_command(
    input: &mut Async<std::net::TcpStream>,
    replies: &flume::Receiver<Vec<u8>>,
    command: &str,
) -> String {
    use futures_lite::io::AsyncWriteExt;
    input
        .write_all(format!("{command}\n").as_bytes())
        .await
        .unwrap();
    future::or(
        async { String::from_utf8(replies.recv_async().await.unwrap()).unwrap() },
        async {
            async_io::Timer::after(Duration::from_secs(5)).await;
            panic!("runtime terminal did not reply to {command}");
        },
    )
    .await
}

mod admission;
mod persistence;
mod physical_control;
mod runtime;
mod virtual_control;
