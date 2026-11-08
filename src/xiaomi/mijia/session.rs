use crate::xiaomi::cloud::CloudError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use isahc::http::header::HeaderValue;
use std::fmt;

/// A complete, validated device session. Both service secrets belong to the
/// same login and are replaced together.
#[derive(Clone, Eq, PartialEq)]
pub struct MijiaSession {
    user_id: String,
    service_token: String,
    ssecurity: String,
    pub(super) security_key: Vec<u8>,
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
