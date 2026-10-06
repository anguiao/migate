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
use futures_lite::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use mqttbytes::v5::{Connect, ConnectProperties, ConnectReturnCode, Packet};
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
    let handshake = async {
        stream
            .write_all(
                &mqtt::encode_packet(&Packet::Connect(connect))
                    .map_err(|_| NotificationError::Protocol)?,
            )
            .await
            .map_err(|_| NotificationError::Io)?;
        stream.flush().await.map_err(|_| NotificationError::Io)?;
        loop {
            if let Some(packet) = frames
                .next_packet()
                .map_err(|_| NotificationError::Protocol)?
            {
                return match packet {
                    Packet::ConnAck(ack) => Ok(ack),
                    _ => Err(NotificationError::Protocol),
                };
            }
            let mut bytes = [0; 4096];
            let len = stream
                .read(&mut bytes)
                .await
                .map_err(|_| NotificationError::Io)?;
            if len == 0 {
                return Err(NotificationError::Disconnected);
            }
            frames
                .push(&bytes[..len])
                .map_err(|_| NotificationError::Protocol)?;
        }
    };
    let ack = future::race(handshake, async {
        async_io::Timer::after(timing.request).await;
        Err(NotificationError::Timeout)
    })
    .await?;
    if ack.code != ConnectReturnCode::Success {
        return Err(
            if matches!(
                ack.code,
                ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized
            ) {
                NotificationError::Unauthorized
            } else {
                NotificationError::Rejected(ack.code as u8)
            },
        );
    }
    if ack.session_present {
        return Err(NotificationError::Protocol);
    }
    let mut max_packet = mqtt::MAX_FRAME_SIZE;
    if let Some(properties) = ack.properties {
        if properties.receive_max == Some(0)
            || properties.max_packet_size == Some(0)
            || properties.authentication_method.is_some()
            || properties.authentication_data.is_some()
        {
            return Err(NotificationError::Protocol);
        }
        max_packet = properties
            .max_packet_size
            .map_or(max_packet, |size| (size as usize).min(max_packet));
        if let Some(seconds) = properties.server_keep_alive {
            timing.keepalive = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
        }
    }
    Ok(NotificationDriver::new(
        stream,
        frames,
        credentials.uid.clone(),
        timing,
        max_packet,
    ))
}
