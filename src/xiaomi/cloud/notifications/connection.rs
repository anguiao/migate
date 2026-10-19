use super::{
    LIMIT, NotificationDriver, NotificationError, NotificationHandle, Timing, Transport,
    request::valid_identifier, tls,
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::{
        cloud::{CLIENT_ID, REGION},
        mqtt,
    },
};
use mqttbytes::v5::{Connect, ConnectProperties, ConnectReturnCode};
use std::time::Duration;

const HOST: &str = "cn-ha.mqtt.io.mi.com";
const PORT: u16 = 8883;

pub async fn connect_notifications(
    credentials: &XiaomiRecord,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    validate_credentials(credentials)?;
    let stream = tls::connect(HOST, PORT).await?;
    start(Box::new(stream), credentials, Timing::default()).await
}

fn validate_credentials(credentials: &XiaomiRecord) -> Result<String, NotificationError> {
    if credentials.region != REGION
        || !valid_identifier(&credentials.uid)
        || credentials.tokens.access_token.is_empty()
        || credentials.tokens.access_token.len() > u16::MAX as usize
        || credentials
            .tokens
            .access_token
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(NotificationError::InvalidInput);
    }
    let uuid = uuid::Uuid::parse_str(&credentials.oauth_client_uuid)
        .map_err(|_| NotificationError::InvalidInput)?;
    Ok(format!("ha.{}", uuid.simple()))
}

pub(super) async fn start(
    mut stream: Box<dyn Transport>,
    credentials: &XiaomiRecord,
    mut timing: Timing,
) -> Result<(NotificationHandle, NotificationDriver), NotificationError> {
    let client_id = validate_credentials(credentials)?;
    let mut connect = Connect::new(client_id);
    connect.set_login(
        CLIENT_ID.to_string(),
        credentials.tokens.access_token.clone(),
    );
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
    if let Some(seconds) = handshake.server_keep_alive {
        timing.keepalive = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
    }
    Ok(NotificationDriver::new(
        stream,
        frames,
        credentials.uid.clone(),
        timing,
        handshake.max_packet,
    ))
}

fn handshake_error(error: mqtt::HandshakeError) -> NotificationError {
    match error {
        mqtt::HandshakeError::Io => NotificationError::Io,
        mqtt::HandshakeError::Protocol => NotificationError::Protocol,
        mqtt::HandshakeError::Timeout => NotificationError::Timeout,
        mqtt::HandshakeError::Disconnected => NotificationError::Disconnected,
        mqtt::HandshakeError::Rejected(code) => {
            if matches!(
                code,
                ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized
            ) {
                NotificationError::Unauthorized
            } else {
                NotificationError::Rejected(code as u8)
            }
        }
    }
}
