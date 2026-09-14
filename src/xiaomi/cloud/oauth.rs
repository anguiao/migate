use super::{CLIENT_ID, CloudError};
use sha1::{Digest as _, Sha1};
use std::fmt;
use url::Url;
use uuid::Uuid;

pub const AUTHORIZATION_URL: &str = "https://account.xiaomi.com/oauth2/authorize";
const CALLBACK_BASE_URL: &str = "http://homeassistant.local:8123";

pub fn validate_saved_redirect_uri(value: &str) -> Result<(), CloudError> {
    let redirect = Url::parse(value)
        .map_err(|_| CloudError::input("validate saved authorization callback"))?;
    let identifier = redirect
        .path()
        .strip_prefix("/api/webhook/")
        .filter(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if redirect.scheme() != "http"
        || redirect.host_str() != Some("homeassistant.local")
        || redirect.port() != Some(8123)
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.query().is_some()
        || redirect.fragment().is_some()
        || identifier.is_none()
    {
        return Err(CloudError::input("validate saved authorization callback"));
    }
    Ok(())
}

pub struct AuthorizationAttempt {
    oauth_client_uuid: String,
    redirect_uri: String,
    state: String,
    authorization_url: String,
}

impl AuthorizationAttempt {
    pub fn new(oauth_client_uuid: Option<&str>) -> Result<Self, CloudError> {
        let uuid = match oauth_client_uuid {
            Some(value) => {
                let parsed = Uuid::parse_str(value)
                    .map_err(|_| CloudError::input("create authorization"))?;
                if parsed.hyphenated().to_string() != value {
                    return Err(CloudError::input("create authorization"));
                }
                value.to_owned()
            }
            None => Uuid::new_v4().hyphenated().to_string(),
        };
        let redirect_uri = format!(
            "{CALLBACK_BASE_URL}/api/webhook/{}",
            Uuid::new_v4().simple()
        );
        let device_id = format!("ha.{uuid}");
        let state = Sha1::digest(format!("d={device_id}").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut url = Url::parse(AUTHORIZATION_URL)
            .map_err(|_| CloudError::protocol("create authorization"))?;
        url.query_pairs_mut()
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("client_id", &CLIENT_ID.to_string())
            .append_pair("response_type", "code")
            .append_pair("device_id", &device_id)
            .append_pair("state", &state)
            .append_pair("skip_confirm", "false");
        Ok(Self {
            oauth_client_uuid: uuid,
            redirect_uri,
            state,
            authorization_url: url.into(),
        })
    }

    pub fn oauth_client_uuid(&self) -> &str {
        &self.oauth_client_uuid
    }
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }
    pub fn state(&self) -> &str {
        &self.state
    }
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn parse_callback(&self, input: &str) -> Result<String, CloudError> {
        let callback = Url::parse(input.trim())
            .map_err(|_| CloudError::input("parse authorization callback"))?;
        let expected = Url::parse(&self.redirect_uri)
            .map_err(|_| CloudError::protocol("parse authorization callback"))?;
        if callback.scheme() != expected.scheme()
            || callback.host_str() != expected.host_str()
            || callback.port_or_known_default() != expected.port_or_known_default()
            || callback.path() != expected.path()
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
        {
            return Err(CloudError::input("parse authorization callback"));
        }
        let mut code = None;
        let mut state = None;
        for (key, value) in callback.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() && !value.is_empty() => code = Some(value.into_owned()),
                "state" if state.is_none() && !value.is_empty() => state = Some(value.into_owned()),
                "code" | "state" | "error" | "error_description" => {
                    return Err(CloudError::input("parse authorization callback"));
                }
                _ => {}
            }
        }
        if state.as_deref() != Some(&self.state) {
            return Err(CloudError::input("parse authorization callback"));
        }
        code.ok_or_else(|| CloudError::input("parse authorization callback"))
    }
}

impl fmt::Debug for AuthorizationAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationAttempt")
            .field("oauth_client_uuid", &self.oauth_client_uuid)
            .field("redirect_uri", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("authorization_url", &"[REDACTED]")
            .finish()
    }
}
