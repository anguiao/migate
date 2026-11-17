use super::MijiaSession;
use crate::xiaomi::{
    cloud::{CloudError, CloudErrorKind},
    http::response_read_error_kind,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const MIJIA_BASE_URL: &str = "https://api.io.mi.com";
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(in crate::xiaomi) struct MijiaClient {
    client: HttpClient,
    pub(super) base_url: String,
    timeout: Duration,
}

impl MijiaClient {
    pub(in crate::xiaomi) fn new() -> Result<Self, CloudError> {
        Self::build(MIJIA_BASE_URL, READ_TIMEOUT)
    }

    fn build(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        let operation = "configure Mijia client";
        let url = Url::parse(base_url).map_err(|_| CloudError::input(operation))?;
        if url.cannot_be_a_base() || url.query().is_some() || url.fragment().is_some() {
            return Err(CloudError::input(operation));
        }
        let client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .build()
            .map_err(|_| CloudError::new(operation, CloudErrorKind::Network))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            timeout,
        })
    }

    pub(super) async fn exchange(
        &self,
        operation: &'static str,
        path: &str,
        session: &MijiaSession,
        data: Value,
        timeout: Duration,
    ) -> Result<Envelope, CloudError> {
        let data = data.to_string();
        let (random, minutes) = Self::nonce(operation)?;
        let (nonce, signature) = signed_fields(&session.security_key, random, minutes, path, &data);
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("data", &data)
            .append_pair("_nonce", &nonce)
            .append_pair("signature", &signature)
            .finish();
        let url = format!("{}{}", self.base_url, path);
        let request = Request::builder()
            .method("POST")
            .uri(url)
            .timeout(timeout.min(self.timeout))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", session.user_agent())
            .header("x-xiaomi-protocal-flag-cli", "PROTOCAL-HTTP2")
            .header(
                "Cookie",
                format!(
                    "userId={}; serviceToken={}",
                    session.user_id(),
                    session.service_token()
                ),
            )
            .body(form)
            .map_err(|_| CloudError::protocol(operation))?;
        let mut response = self.client.send_async(request).await.map_err(|error| {
            CloudError::new(
                operation,
                if error.is_timeout() {
                    CloudErrorKind::Timeout
                } else {
                    CloudErrorKind::Network
                },
            )
        })?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(CloudError::new(operation, CloudErrorKind::Unauthorized));
        }
        if status != 200 {
            return Err(CloudError::new(
                operation,
                CloudErrorKind::HttpStatus(status),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| CloudError::new(operation, response_read_error_kind(&error)))?;
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| CloudError::protocol(operation))?;
        if envelope.code != 0 {
            if envelope.message.as_str() == Some("invalid signature") {
                return Err(CloudError::invalid_signature(operation));
            }
            let kind = match envelope.message.as_str() {
                Some("SERVICETOKEN_EXPIRED" | "auth err") => CloudErrorKind::Unauthorized,
                _ if matches!(envelope.code, 2 | 3) => CloudErrorKind::Unauthorized,
                _ => CloudErrorKind::Business(envelope.code),
            };
            return Err(CloudError::new(operation, kind));
        }
        Ok(envelope)
    }

    fn nonce(operation: &'static str) -> Result<([u8; 8], u32), CloudError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CloudError::protocol(operation))?
            .as_secs();
        let minutes = u32::try_from(seconds / 60).map_err(|_| CloudError::protocol(operation))?;
        Ok((rand::random(), minutes))
    }
}

#[derive(Deserialize)]
pub(super) struct Envelope {
    code: i64,
    #[serde(default)]
    message: Value,
    #[serde(default)]
    pub(super) id: Option<u64>,
    #[serde(default, deserialize_with = "present_raw")]
    pub(super) result: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present_raw")]
    pub(super) error: Option<Box<RawValue>>,
}

fn present_raw<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

impl MijiaClient {
    pub(super) async fn protected_post<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        session: &MijiaSession,
        body: Value,
    ) -> Result<T, CloudError> {
        self.protected_post_with_timeout(operation, path, session, body, READ_TIMEOUT)
            .await
    }
    pub(super) async fn protected_post_with_timeout<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        session: &MijiaSession,
        body: Value,
        timeout: Duration,
    ) -> Result<T, CloudError> {
        let raw = self
            .exchange(operation, path, session, body, timeout)
            .await?
            .result
            .ok_or_else(|| CloudError::protocol(operation))?;
        serde_json::from_str(raw.get()).map_err(|_| CloudError::protocol(operation))
    }
    pub(super) async fn protected_post_raw(
        &self,
        operation: &'static str,
        path: &str,
        session: &MijiaSession,
        body: Value,
    ) -> Result<Box<RawValue>, CloudError> {
        self.exchange(operation, path, session, body, READ_TIMEOUT)
            .await?
            .result
            .ok_or_else(|| CloudError::protocol(operation))
    }
}

fn signed_fields(
    secret: &[u8],
    random: [u8; 8],
    minutes: u32,
    path: &str,
    data: &str,
) -> (String, String) {
    let mut nonce_bytes = [0; 12];
    nonce_bytes[..8].copy_from_slice(&random);
    nonce_bytes[8..].copy_from_slice(&minutes.to_be_bytes());
    let nonce = STANDARD.encode(nonce_bytes);
    let signed_nonce = Sha256::digest([secret, &nonce_bytes].concat());
    let sign = format!(
        "{}&{}&{}&data={}",
        path.strip_prefix("/app")
            .expect("device path begins with /app"),
        STANDARD.encode(signed_nonce),
        nonce,
        data
    );
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&signed_nonce).expect("HMAC accepts keys of any length");
    mac.update(sign.as_bytes());
    (nonce, STANDARD.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests;
