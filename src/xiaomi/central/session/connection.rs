use super::super::{CentralError, tls};
use super::{LIMIT, SessionDriver, SessionHandle, Timing, Transport, request::valid_identifier};
use crate::storage::XiaomiRecord;
use crate::xiaomi::mqtt::{self, HandshakeError};
use mqttbytes::v5::{Connect, ConnectProperties};
use std::{net::SocketAddr, time::Duration};

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
        max_packet_size: Some(mqtt::MAX_FRAME_SIZE as u32),
        topic_alias_max: Some(0),
        request_response_info: None,
        request_problem_info: Some(0),
        user_properties: Vec::new(),
        authentication_method: None,
        authentication_data: None,
    });
    let mut frames = mqtt::Frames::default();
    let handshake = mqtt::handshake(&mut stream, &mut frames, connect, timing.request)
        .await
        .map_err(handshake_error)?;
    // The gateway must accept exactly-once business exchanges.
    if handshake.max_qos.is_some_and(|max_qos| max_qos < 2) {
        return Err(CentralError::Protocol);
    }
    if let Some(seconds) = handshake.server_keep_alive {
        timing.keepalive = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
    }
    let receive_max = usize::from(handshake.receive_max.unwrap_or(LIMIT as u16)).min(LIMIT);
    SessionDriver::new(
        stream,
        frames,
        client_id,
        timing,
        handshake.max_packet,
        receive_max,
    )
}

fn handshake_error(error: HandshakeError) -> CentralError {
    match error {
        HandshakeError::Io => CentralError::Io,
        HandshakeError::Protocol => CentralError::Protocol,
        HandshakeError::Timeout => CentralError::Timeout,
        HandshakeError::Disconnected => CentralError::Disconnected,
        HandshakeError::Rejected(code) => CentralError::Rejected(code as i64),
    }
}
