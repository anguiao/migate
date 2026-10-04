use super::super::{CentralError, tls, wire};
use super::{LIMIT, SessionDriver, SessionHandle, Timing, Transport, request::valid_identifier};
use crate::storage::XiaomiRecord;
use futures_lite::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use mqttbytes::v5::{Connect, ConnectProperties, ConnectReturnCode, Packet};
use std::{net::SocketAddr, time::Duration};

const MAX_PACKET: usize = wire::MAX_FRAME_SIZE;

pub async fn connect(
    address: SocketAddr,
    credentials: &XiaomiRecord,
) -> Result<(SessionHandle, SessionDriver), CentralError> {
    let stream = tls::connect_tls(address, credentials).await?;
    start(
        Box::new(stream),
        &credentials.virtual_did,
        Timing::default(),
    )
    .await
}
pub(in crate::xiaomi::central) async fn start(
    mut stream: Box<dyn Transport>,
    client_id: &str,
    mut timing: Timing,
) -> Result<(SessionHandle, SessionDriver), CentralError> {
    if !valid_identifier(client_id) {
        return Err(CentralError::InvalidInput);
    }
    let mut connect = Connect::new(client_id);
    connect.keep_alive = timing.keepalive.map_or(0, |duration| {
        duration.as_secs().clamp(1, u64::from(u16::MAX)) as u16
    });
    connect.properties = Some(ConnectProperties {
        session_expiry_interval: Some(0),
        receive_maximum: Some(LIMIT as u16),
        max_packet_size: Some(MAX_PACKET as u32),
        topic_alias_max: Some(0),
        request_response_info: None,
        request_problem_info: Some(0),
        user_properties: Vec::new(),
        authentication_method: None,
        authentication_data: None,
    });
    let mut frames = wire::Frames::default();
    let handshake = async {
        stream
            .write_all(&wire::encode_packet(&Packet::Connect(connect))?)
            .await
            .map_err(|_| CentralError::Io)?;
        stream.flush().await.map_err(|_| CentralError::Io)?;
        loop {
            if let Some(packet) = frames.next_packet()? {
                return match packet {
                    Packet::ConnAck(ack) => Ok(ack),
                    _ => Err(CentralError::Protocol),
                };
            }
            let mut bytes = [0; 4096];
            let len = stream
                .read(&mut bytes)
                .await
                .map_err(|_| CentralError::Io)?;
            if len == 0 {
                return Err(CentralError::Disconnected);
            }
            frames.push(&bytes[..len])?;
        }
    };
    let ack = future::race(handshake, async {
        async_io::Timer::after(timing.request).await;
        Err(CentralError::Timeout)
    })
    .await?;
    if ack.code != ConnectReturnCode::Success {
        return Err(CentralError::Rejected(ack.code as i64));
    }
    if ack.session_present {
        return Err(CentralError::Protocol);
    }
    let mut max_packet = MAX_PACKET;
    let mut receive_max = LIMIT;
    if let Some(properties) = ack.properties {
        if properties.max_qos.is_some_and(|qos| qos < 2)
            || properties.receive_max == Some(0)
            || properties.max_packet_size == Some(0)
            || properties.authentication_method.is_some()
            || properties.authentication_data.is_some()
        {
            return Err(CentralError::Protocol);
        }
        max_packet = properties
            .max_packet_size
            .map_or(MAX_PACKET, |size| (size as usize).min(MAX_PACKET));
        receive_max = usize::from(properties.receive_max.unwrap_or(LIMIT as u16)).min(LIMIT);
        if let Some(seconds) = properties.server_keep_alive {
            timing.keepalive = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
        }
    }
    SessionDriver::new(stream, frames, client_id, timing, max_packet, receive_max)
}
