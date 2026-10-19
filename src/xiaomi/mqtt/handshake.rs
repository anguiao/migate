use super::{Frames, MAX_FRAME_SIZE, encode_packet};
use futures_lite::{
    future,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};
use mqttbytes::v5::{Connect, ConnectReturnCode, Packet};
use std::time::Duration;

/// Common handshake failures. Drivers map these onto their own error types and
/// keep authentication, credential and capability policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::xiaomi) enum HandshakeError {
    Io,
    Protocol,
    Timeout,
    Disconnected,
    /// The server refused the connection instead of accepting it.
    Rejected(ConnectReturnCode),
}

/// Common CONNACK facts after the shared protocol checks. `receive_max` and
/// `max_qos` are server limits the drivers may enforce with their own policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::xiaomi) struct Handshake {
    pub(in crate::xiaomi) max_packet: usize,
    pub(in crate::xiaomi) receive_max: Option<u16>,
    pub(in crate::xiaomi) max_qos: Option<u8>,
    pub(in crate::xiaomi) server_keep_alive: Option<u16>,
}

/// Write CONNECT, flush it and read the CONNACK within `response_timeout`.
/// Capability requirements specific to a source stay with the driver.
pub(in crate::xiaomi) async fn handshake<S>(
    stream: &mut S,
    frames: &mut Frames,
    connect: Connect,
    response_timeout: Duration,
) -> Result<Handshake, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let exchange = async {
        stream
            .write_all(
                &encode_packet(&Packet::Connect(connect)).map_err(|_| HandshakeError::Protocol)?,
            )
            .await
            .map_err(|_| HandshakeError::Io)?;
        stream.flush().await.map_err(|_| HandshakeError::Io)?;
        loop {
            if let Some(packet) = frames.next_packet().map_err(|_| HandshakeError::Protocol)? {
                return match packet {
                    Packet::ConnAck(ack) => Ok(ack),
                    _ => Err(HandshakeError::Protocol),
                };
            }
            let mut bytes = [0; 4096];
            let len = stream
                .read(&mut bytes)
                .await
                .map_err(|_| HandshakeError::Io)?;
            if len == 0 {
                return Err(HandshakeError::Disconnected);
            }
            frames
                .push(&bytes[..len])
                .map_err(|_| HandshakeError::Protocol)?;
        }
    };
    let ack = future::race(exchange, async {
        async_io::Timer::after(response_timeout).await;
        Err(HandshakeError::Timeout)
    })
    .await?;
    validate(ack)
}

fn validate(ack: mqttbytes::v5::ConnAck) -> Result<Handshake, HandshakeError> {
    if ack.code != ConnectReturnCode::Success {
        return Err(HandshakeError::Rejected(ack.code));
    }
    if ack.session_present {
        return Err(HandshakeError::Protocol);
    }
    let mut max_packet = MAX_FRAME_SIZE;
    let mut receive_max = None;
    let mut max_qos = None;
    let mut server_keep_alive = None;
    if let Some(properties) = ack.properties {
        if properties.receive_max == Some(0)
            || properties.max_packet_size == Some(0)
            || properties.authentication_method.is_some()
            || properties.authentication_data.is_some()
        {
            return Err(HandshakeError::Protocol);
        }
        receive_max = properties.receive_max;
        max_qos = properties.max_qos;
        server_keep_alive = properties.server_keep_alive;
        max_packet = properties
            .max_packet_size
            .map_or(MAX_FRAME_SIZE, |size| (size as usize).min(MAX_FRAME_SIZE));
    }
    Ok(Handshake {
        max_packet,
        receive_max,
        max_qos,
        server_keep_alive,
    })
}
