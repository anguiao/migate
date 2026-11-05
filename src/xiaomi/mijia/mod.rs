use crate::xiaomi::cloud::{
    CloudError, CloudErrorKind, client::response_read_error_kind, device::DeviceTransport,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
    http::header::HeaderValue,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

pub const MIJIA_BASE_URL: &str = "https://api.io.mi.com";
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
static RPC_IDS: OnceLock<AtomicU32> = OnceLock::new();

/// A complete, validated device session. Both service secrets belong to the
/// same login and are replaced together.
#[derive(Clone, Eq, PartialEq)]
pub struct MijiaSession {
    user_id: String,
    service_token: String,
    ssecurity: String,
    security_key: Vec<u8>,
    user_agent: String,
}

impl MijiaSession {
    pub fn new(
        user_id: String,
        service_token: String,
        ssecurity: String,
        user_agent: String,
    ) -> Result<Self, CloudError> {
        let operation = "validate Mijia session";
        let security_key = STANDARD
            .decode(&ssecurity)
            .map_err(|_| CloudError::input(operation))?;
        if !cookie_value(&user_id)
            || !cookie_value(&service_token)
            || security_key.is_empty()
            || user_agent.is_empty()
            || HeaderValue::from_str(&user_agent).is_err()
        {
            return Err(CloudError::input(operation));
        }
        Ok(Self {
            user_id,
            service_token,
            ssecurity,
            security_key,
            user_agent,
        })
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn service_token(&self) -> &str {
        &self.service_token
    }
    pub fn ssecurity(&self) -> &str {
        &self.ssecurity
    }
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}

impl fmt::Debug for MijiaSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MijiaSession")
            .field("user_id", &self.user_id)
            .field("service_token", &"[REDACTED]")
            .field("ssecurity", &"[REDACTED]")
            .field("user_agent", &self.user_agent)
            .finish()
    }
}

fn cookie_value(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(
            |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
        )
}

#[derive(Clone)]
pub struct MijiaClient {
    client: HttpClient,
    base_url: String,
    timeout: Duration,
    #[cfg(test)]
    fixed_nonce: Option<([u8; 8], u32)>,
}

impl MijiaClient {
    pub fn new() -> Result<Self, CloudError> {
        Self::build(MIJIA_BASE_URL, READ_TIMEOUT)
    }

    #[cfg(test)]
    pub(in crate::xiaomi) fn for_test(base: &str, timeout: Duration) -> Self {
        assert_eq!(Url::parse(base).unwrap().host_str(), Some("127.0.0.1"));
        let mut client = Self::build(base, timeout).unwrap();
        client.client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .proxy(None)
            .build()
            .unwrap();
        client
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
            #[cfg(test)]
            fixed_nonce: None,
        })
    }

    async fn exchange(
        &self,
        operation: &'static str,
        path: &str,
        session: &MijiaSession,
        data: Value,
        timeout: Duration,
    ) -> Result<Envelope, CloudError> {
        let data = data.to_string();
        let (random, minutes) = self.nonce(operation)?;
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
            .header("User-Agent", &session.user_agent)
            .header("x-xiaomi-protocal-flag-cli", "PROTOCAL-HTTP2")
            .header(
                "Cookie",
                format!(
                    "userId={}; serviceToken={}",
                    session.user_id, session.service_token
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

    fn nonce(&self, operation: &'static str) -> Result<([u8; 8], u32), CloudError> {
        #[cfg(test)]
        if let Some(nonce) = self.fixed_nonce {
            return Ok(nonce);
        }
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CloudError::protocol(operation))?
            .as_secs();
        let minutes = u32::try_from(seconds / 60).map_err(|_| CloudError::protocol(operation))?;
        Ok((rand::random(), minutes))
    }

    /// Returns the correlated native action code and output without treating
    /// acceptance as a state observation.
    pub async fn action(
        &self,
        session: &MijiaSession,
        did: &str,
        siid: u32,
        aiid: u32,
        input: Vec<Value>,
    ) -> Result<ActionResult, CloudError> {
        let operation = "invoke Xiaomi action";
        if !valid_text(did) || siid == 0 || aiid == 0 {
            return Err(CloudError::input(operation));
        }
        let result: Value = self
            .protected_post_with_timeout(
                operation,
                "/app/miotspec/action",
                session,
                json!({"params":{"did":did,"siid":siid,"aiid":aiid,"in":input}}),
                WRITE_TIMEOUT,
            )
            .await?;
        if result.get("did").and_then(Value::as_str) != Some(did)
            || result.get("siid").and_then(Value::as_u64) != Some(u64::from(siid))
            || result.get("aiid").and_then(Value::as_u64) != Some(u64::from(aiid))
        {
            return Err(CloudError::protocol(operation));
        }
        let code = result
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| CloudError::protocol(operation))?;
        Ok(ActionResult { code, result })
    }

    /// Sends exactly one RPC request and preserves its native result or error.
    pub async fn rpc(
        &self,
        session: &MijiaSession,
        did: &str,
        method: &str,
        params: Value,
    ) -> Result<RpcOutcome, CloudError> {
        let operation = "invoke Xiaomi RPC";
        if !valid_text(did) || matches!(did, "." | "..") || !valid_text(method) {
            return Err(CloudError::input(operation));
        }
        let mut url = Url::parse(&format!("{}/app/home/rpc/", self.base_url))
            .map_err(|_| CloudError::protocol(operation))?;
        url.path_segments_mut()
            .map_err(|_| CloudError::protocol(operation))?
            .pop_if_empty()
            .push(did);
        let path = url.path();
        let id = RPC_IDS
            .get_or_init(|| AtomicU32::new(rand::random()))
            .fetch_add(1, Ordering::Relaxed)
            % 0x8000_0000
            + 1;
        let envelope = self
            .exchange(
                operation,
                path,
                session,
                json!({"id":id,"method":method,"params":params}),
                READ_TIMEOUT,
            )
            .await?;
        if envelope.id != Some(u64::from(id)) {
            return Err(CloudError::protocol(operation));
        }
        let decode = |raw: Box<RawValue>| {
            serde_json::from_str::<Value>(raw.get()).map_err(|_| CloudError::protocol(operation))
        };
        match (envelope.result, envelope.error) {
            (Some(value), None) => Ok(RpcOutcome::Result(decode(value)?)),
            (None, Some(error)) => {
                let error = decode(error)?;
                if error.get("code").and_then(Value::as_i64).is_none() {
                    return Err(CloudError::protocol(operation));
                }
                Ok(RpcOutcome::Error(error))
            }
            _ => Err(CloudError::protocol(operation)),
        }
    }
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

#[derive(Clone, Eq, PartialEq)]
pub struct ActionResult {
    pub code: i64,
    pub result: Value,
}
impl fmt::Debug for ActionResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActionResult")
            .field("code", &self.code)
            .field("result", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum RpcOutcome {
    Result(Value),
    Error(Value),
}
impl fmt::Debug for RpcOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Result(_) => formatter.write_str("RpcOutcome::Result([REDACTED])"),
            Self::Error(_) => formatter.write_str("RpcOutcome::Error([REDACTED])"),
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    message: Value,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default, deserialize_with = "present_raw")]
    result: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present_raw")]
    error: Option<Box<RawValue>>,
}

fn present_raw<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

impl DeviceTransport for MijiaClient {
    type Credential = MijiaSession;
    fn read_property_path(&self) -> &'static str {
        "/app/miotspec/prop/get"
    }
    fn write_property_path(&self) -> &'static str {
        "/app/miotspec/prop/set"
    }
    async fn protected_post_with_timeout<T: DeserializeOwned>(
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
    async fn protected_post_raw(
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
