use crate::storage::TokenSet;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
};
use serde_json::{Map, Value, json};
use sha1::{Digest as _, Sha1};
use std::{collections::HashSet, error::Error as StdError, fmt, time::Duration};
use url::Url;
use uuid::Uuid;

pub const REGION: &str = "cn";
pub const CLIENT_ID: &str = "2882303761520251711";
pub const AUTHORIZATION_URL: &str = "https://account.xiaomi.com/oauth2/authorize";
pub const CLOUD_BASE_URL: &str = "https://ha.api.io.mi.com";
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
            .append_pair("client_id", CLIENT_ID)
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

#[derive(Clone, Eq, PartialEq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

impl TokenResponse {
    pub fn to_token_set(&self, completed_at: i64) -> Result<TokenSet, CloudError> {
        validate_token(&self.access_token, "calculate token expiry")?;
        validate_token(&self.refresh_token, "calculate token expiry")?;
        if self.expires_in <= 0 {
            return Err(CloudError::protocol("calculate token expiry"));
        }
        let expires_at = completed_at
            .checked_add(self.expires_in)
            .ok_or_else(|| CloudError::protocol("calculate token expiry"))?;
        let refresh_offset = self
            .expires_in
            .checked_mul(7)
            .and_then(|value| value.checked_div(10))
            .ok_or_else(|| CloudError::protocol("calculate token expiry"))?;
        let refresh_at = completed_at
            .checked_add(refresh_offset)
            .ok_or_else(|| CloudError::protocol("calculate token expiry"))?;
        Ok(TokenSet {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at,
            refresh_at,
        })
    }
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HomePage {
    pub uid: Option<String>,
    pub dids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloudErrorKind {
    Unauthorized,
    HttpStatus(u16),
    Business(i64),
    Protocol,
    Network,
    Timeout,
    InvalidInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudError {
    operation: &'static str,
    kind: CloudErrorKind,
    oauth_code: Option<i64>,
}

impl CloudError {
    fn new(operation: &'static str, kind: CloudErrorKind) -> Self {
        Self {
            operation,
            kind,
            oauth_code: None,
        }
    }
    fn input(operation: &'static str) -> Self {
        Self::new(operation, CloudErrorKind::InvalidInput)
    }
    fn protocol(operation: &'static str) -> Self {
        Self::new(operation, CloudErrorKind::Protocol)
    }
    pub fn operation(&self) -> &'static str {
        self.operation
    }
    pub fn kind(&self) -> &CloudErrorKind {
        &self.kind
    }
    pub fn is_unauthorized(&self) -> bool {
        self.kind == CloudErrorKind::Unauthorized
    }
    pub fn is_timeout(&self) -> bool {
        self.kind == CloudErrorKind::Timeout
    }
    pub fn http_status(&self) -> Option<u16> {
        if let CloudErrorKind::HttpStatus(value) = self.kind {
            Some(value)
        } else {
            None
        }
    }
}

impl fmt::Display for CloudError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Failed to {}: ", self.operation)?;
        match self.kind {
            CloudErrorKind::Unauthorized => {
                formatter.write_str("authorization was rejected (HTTP 401)")
            }
            CloudErrorKind::HttpStatus(status) => write!(formatter, "HTTP status {status}"),
            CloudErrorKind::Business(code) => {
                write!(formatter, "cloud business code {code}")?;
                if let Some(oauth_code) = self.oauth_code {
                    write!(formatter, " (OAuth error {oauth_code}")?;
                    match oauth_code {
                        96002 => formatter.write_str(": missing or invalid request parameters")?,
                        96013 => formatter.write_str(": invalid authorization code")?,
                        _ => {}
                    }
                    formatter.write_str(")")?;
                }
                Ok(())
            }
            CloudErrorKind::Protocol => formatter.write_str("invalid cloud response"),
            CloudErrorKind::Network => formatter.write_str("network request failed"),
            CloudErrorKind::Timeout => formatter.write_str("network request timed out"),
            CloudErrorKind::InvalidInput => formatter.write_str("invalid input"),
        }
    }
}

impl StdError for CloudError {}

pub struct CloudClient {
    client: HttpClient,
    base_url: String,
}

impl CloudClient {
    pub fn new() -> Result<Self, CloudError> {
        Self::build(CLOUD_BASE_URL, Duration::from_secs(30))
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        Self::build(base_url, timeout)
    }

    fn build(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        let parsed =
            Url::parse(base_url).map_err(|_| CloudError::input("configure cloud client"))?;
        if parsed.cannot_be_a_base() || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(CloudError::input("configure cloud client"));
        }
        let client = HttpClient::builder()
            .timeout(timeout)
            .redirect_policy(RedirectPolicy::None)
            .build()
            .map_err(|_| CloudError::new("configure cloud client", CloudErrorKind::Network))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    pub async fn exchange_token(
        &self,
        attempt: &AuthorizationAttempt,
        code: &str,
    ) -> Result<TokenResponse, CloudError> {
        if code.is_empty() {
            return Err(CloudError::input("exchange authorization code"));
        }
        self.token_request(
            "exchange authorization code",
            json!({
                "client_id": 2_882_303_761_520_251_711_u64,
                "redirect_uri": attempt.redirect_uri(),
                "code": code,
                "device_id": format!("ha.{}", attempt.oauth_client_uuid()),
            }),
        )
        .await
    }

    pub async fn refresh_token(
        &self,
        oauth_client_uuid: &str,
        redirect_uri: &str,
        refresh_token: &str,
    ) -> Result<TokenResponse, CloudError> {
        if Uuid::parse_str(oauth_client_uuid).is_err()
            || redirect_uri.is_empty()
            || refresh_token.is_empty()
        {
            return Err(CloudError::input("refresh access token"));
        }
        self.token_request(
            "refresh access token",
            json!({
                "client_id": 2_882_303_761_520_251_711_u64,
                "redirect_uri": redirect_uri,
                "refresh_token": refresh_token,
            }),
        )
        .await
    }

    async fn token_request(
        &self,
        operation: &'static str,
        data: Value,
    ) -> Result<TokenResponse, CloudError> {
        let mut url = self.endpoint("/app/v2/ha/oauth/get_token", operation)?;
        url.query_pairs_mut().append_pair("data", &data.to_string());
        let request = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(String::new())
            .map_err(|_| CloudError::protocol(operation))?;
        let value = self.send(request, operation).await?;
        let result = result_object(&value, operation).map_err(|mut error| {
            if matches!(error.kind, CloudErrorKind::Business(_)) {
                error.oauth_code = oauth_error_code(&value);
            }
            error
        })?;
        let access_token = token_string(result, "access_token", operation)?;
        let refresh_token = token_string(result, "refresh_token", operation)?;
        let expires_in = result
            .get("expires_in")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| CloudError::protocol(operation))?;
        Ok(TokenResponse {
            access_token,
            refresh_token,
            expires_in,
        })
    }

    pub async fn get_home(&self, access_token: &str) -> Result<HomePage, CloudError> {
        let operation = "read Xiaomi homes";
        let value = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit": 150, "fetch_share": false, "fetch_share_dev": false, "plat_form": 0, "app_ver": 9
        })).await?;
        let result = result_object(&value, operation)?;
        let homes = result
            .get("homelist")
            .and_then(Value::as_array)
            .ok_or_else(|| CloudError::protocol(operation))?;
        let mut uid = None;
        let mut dids = Vec::new();
        let mut seen = HashSet::new();
        for home in homes {
            let home = home
                .as_object()
                .ok_or_else(|| CloudError::protocol(operation))?;
            if uid.is_none() {
                uid = parse_uid(home.get("uid"), operation)?;
            }
            collect_dids(home.get("dids"), &mut dids, &mut seen, operation)?;
            if let Some(rooms) = home.get("roomlist") {
                for room in rooms
                    .as_array()
                    .ok_or_else(|| CloudError::protocol(operation))?
                {
                    let room = room
                        .as_object()
                        .ok_or_else(|| CloudError::protocol(operation))?;
                    collect_dids(room.get("dids"), &mut dids, &mut seen, operation)?;
                }
            }
        }
        dids.truncate(150);
        Ok(HomePage { uid, dids })
    }

    pub async fn get_devices(&self, access_token: &str, dids: &[String]) -> Result<(), CloudError> {
        if dids.is_empty() {
            return Ok(());
        }
        let operation = "read Xiaomi devices";
        let value = self.protected_post(operation, "/app/v2/home/device_list_page", access_token, json!({
            "limit": 200, "get_split_device": true, "get_third_device": true, "dids": &dids[..dids.len().min(150)]
        })).await?;
        let result = result_object(&value, operation)?;
        let list = result
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(|| CloudError::protocol(operation))?;
        if list.iter().any(|device| !device.is_object()) {
            return Err(CloudError::protocol(operation));
        }
        Ok(())
    }

    pub async fn get_certificate(
        &self,
        access_token: &str,
        csr_pem: &str,
    ) -> Result<String, CloudError> {
        if csr_pem.is_empty() {
            return Err(CloudError::input("request gateway certificate"));
        }
        let operation = "request gateway certificate";
        let value = self
            .protected_post(
                operation,
                "/app/v2/ha/oauth/get_central_crt",
                access_token,
                json!({
                    "csr": STANDARD.encode(csr_pem.as_bytes())
                }),
            )
            .await?;
        nonempty_string(result_object(&value, operation)?, "cert", operation)
    }

    async fn protected_post(
        &self,
        operation: &'static str,
        path: &str,
        access_token: &str,
        body: Value,
    ) -> Result<Value, CloudError> {
        if access_token.is_empty() {
            return Err(CloudError::input(operation));
        }
        let url = self.endpoint(path, operation)?;
        let request = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("Content-Type", "application/json")
            .header("X-Client-BizId", "haapi")
            .header("X-Client-AppId", CLIENT_ID)
            .header("Authorization", format!("Bearer{access_token}"))
            .body(body.to_string())
            .map_err(|_| CloudError::protocol(operation))?;
        self.send(request, operation).await
    }

    fn endpoint(&self, path: &str, operation: &'static str) -> Result<Url, CloudError> {
        Url::parse(&format!("{}{path}", self.base_url)).map_err(|_| CloudError::protocol(operation))
    }

    async fn send(
        &self,
        request: Request<String>,
        operation: &'static str,
    ) -> Result<Value, CloudError> {
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
        let body = response
            .bytes()
            .await
            .map_err(|error| CloudError::new(operation, response_read_error_kind(&error)))?;
        serde_json::from_slice(&body).map_err(|_| CloudError::protocol(operation))
    }
}

fn oauth_error_code(value: &Value) -> Option<i64> {
    let message = value.get("message")?.as_str()?;
    let details: Value = serde_json::from_str(message).ok()?;
    details.get("error")?.as_i64()
}

fn result_object<'a>(
    value: &'a Value,
    operation: &'static str,
) -> Result<&'a Map<String, Value>, CloudError> {
    let code = value
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| CloudError::protocol(operation))?;
    if code != 0 {
        return Err(CloudError::new(operation, CloudErrorKind::Business(code)));
    }
    value
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn nonempty_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    result
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CloudError::protocol(operation))
}

fn token_string(
    result: &Map<String, Value>,
    key: &str,
    operation: &'static str,
) -> Result<String, CloudError> {
    let value = nonempty_string(result, key, operation)?;
    validate_token(&value, operation)?;
    Ok(value)
}

fn validate_token(value: &str, operation: &'static str) -> Result<(), CloudError> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(CloudError::protocol(operation));
    }
    Ok(())
}

fn parse_uid(value: Option<&Value>, operation: &'static str) -> Result<Option<String>, CloudError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
            Ok(Some(value.to_string()))
        }
        Some(Value::Number(_)) => Err(CloudError::protocol(operation)),
        _ => Ok(None),
    }
}

fn response_read_error_kind(error: &std::io::Error) -> CloudErrorKind {
    if error.kind() == std::io::ErrorKind::TimedOut {
        return CloudErrorKind::Timeout;
    }
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<isahc::Error>()
            .is_some_and(isahc::Error::is_timeout)
        {
            return CloudErrorKind::Timeout;
        }
        source = current.source();
    }
    CloudErrorKind::Network
}

fn collect_dids(
    value: Option<&Value>,
    dids: &mut Vec<String>,
    seen: &mut HashSet<String>,
    operation: &'static str,
) -> Result<(), CloudError> {
    let Some(value) = value else {
        return Ok(());
    };
    for did in value
        .as_array()
        .ok_or_else(|| CloudError::protocol(operation))?
    {
        let did = did
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CloudError::protocol(operation))?;
        if seen.insert(did.to_owned()) && dids.len() < 150 {
            dids.push(did.to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xiaomi::test_support::{MockResponse, mock_server};
    use base64::engine::general_purpose::STANDARD;
    use futures_lite::future;
    use serde_json::{Value, json};
    use std::time::Duration;
    use url::Url;

    #[test]
    fn authorization_state_matches_upstream_device_binding() {
        for (uuid, expected_state) in [
            (
                "550e8400-e29b-41d4-a716-446655440000",
                "139e3d81ebea72caaec5f4bb548d66e83ecce75c",
            ),
            (
                "550e8400-e29b-41d4-a716-446655440001",
                "510dfb7675e97409da88b563551ea8d59fdc43f6",
            ),
        ] {
            let attempt = AuthorizationAttempt::new(Some(uuid)).unwrap();
            let url = Url::parse(attempt.authorization_url()).unwrap();
            let query = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(query["device_id"], format!("ha.{uuid}"));
            assert_eq!(query["state"], expected_state);
            assert_eq!(attempt.state(), expected_state);
        }
    }

    #[test]
    fn authorization_callbacks_are_bound_to_unique_paths_and_expected_state() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let first = AuthorizationAttempt::new(Some(uuid)).unwrap();
        let second = AuthorizationAttempt::new(Some(uuid)).unwrap();
        assert_eq!(first.oauth_client_uuid(), uuid);
        assert_eq!(first.state(), second.state());
        assert_ne!(first.redirect_uri(), second.redirect_uri());
        let url = Url::parse(first.authorization_url()).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["device_id"], format!("ha.{uuid}"));
        assert_eq!(query["skip_confirm"], "false");

        let callback = format!(
            "{}?code=secret-code&state={}",
            first.redirect_uri(),
            first.state()
        );
        assert_eq!(
            first.parse_callback(&format!(" \n{callback}\t")).unwrap(),
            "secret-code"
        );
        for invalid in [
            callback.replacen("http://", "https://", 1),
            callback.replacen("homeassistant.local", "example.invalid", 1),
            callback.replacen(":8123", ":8124", 1),
            callback.replacen("/api/webhook/", "/wrong/", 1),
            format!(
                "{}?code=x&state={}&state={}",
                first.redirect_uri(),
                first.state(),
                first.state()
            ),
            format!("{}?code=&state={}", first.redirect_uri(), first.state()),
            format!("{}?code=x&state=", first.redirect_uri()),
            format!(
                "{}?code=x&code=y&state={}",
                first.redirect_uri(),
                first.state()
            ),
            format!("{}?code=x&state=wrong", first.redirect_uri()),
            format!(
                "{}?error=denied&state={}",
                first.redirect_uri(),
                first.state()
            ),
            format!(
                "{}?code=x&state={}#fragment",
                first.redirect_uri(),
                first.state()
            ),
            callback.replacen("http://", "http://user@", 1),
            format!("{}?code=x&state={}", second.redirect_uri(), second.state()),
        ] {
            let error = first.parse_callback(&invalid).unwrap_err();
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains(&invalid));
            assert!(!diagnostic.contains(first.state()));
        }
    }

    #[test]
    fn token_requests_match_wire_protocol_and_calculate_times() {
        let body = r#"{"code":0,"result":{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":1000}}"#;
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, body),
            MockResponse::json(200, body),
        ]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let attempt =
            AuthorizationAttempt::new(Some("550e8400-e29b-41d4-a716-446655440000")).unwrap();
        let authorization_url = Url::parse(attempt.authorization_url()).unwrap();
        let authorization_query = authorization_url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        let exchanged = future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap();
        let refreshed = future::block_on(client.refresh_token(
            attempt.oauth_client_uuid(),
            attempt.redirect_uri(),
            "refresh-input",
        ))
        .unwrap();
        assert_eq!(exchanged.to_token_set(100).unwrap().refresh_at, 800);
        assert_eq!(exchanged.to_token_set(100).unwrap().expires_at, 1100);
        assert!(!format!("{exchanged:?}").contains("secret"));
        for (request, expected_key) in [
            (requests.recv().unwrap(), "code"),
            (requests.recv().unwrap(), "refresh_token"),
        ] {
            assert!(
                request
                    .target
                    .starts_with("/app/v2/ha/oauth/get_token?data=")
            );
            let url = Url::parse(&format!("http://local{}", request.target)).unwrap();
            let data = url.query_pairs().find(|(key, _)| key == "data").unwrap().1;
            let data: Value = serde_json::from_str(&data).unwrap();
            assert_eq!(data["client_id"], json!(2_882_303_761_520_251_711_u64));
            assert_eq!(
                data["redirect_uri"].as_str().unwrap(),
                authorization_query["redirect_uri"]
            );
            if expected_key == "code" {
                assert_eq!(
                    data["device_id"].as_str().unwrap(),
                    authorization_query["device_id"]
                );
            }
            assert!(data.get(expected_key).is_some());
            assert!(data.get("grant_type").is_none());
        }
        assert_eq!(refreshed.expires_in, 1000);

        for invalid in [
            TokenResponse {
                access_token: String::new(),
                refresh_token: "refresh".into(),
                expires_in: 1,
            },
            TokenResponse {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_in: 0,
            },
            TokenResponse {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_in: i64::MAX,
            },
        ] {
            assert!(invalid.to_token_set(1).is_err());
        }
    }

    #[test]
    fn token_response_rejects_whitespace_and_control_characters() {
        for result in [
            json!({"access_token":" access","refresh_token":"refresh","expires_in":1}),
            json!({"access_token":"access","refresh_token":"refresh\nsecret","expires_in":1}),
        ] {
            let body = json!({"code":0,"result":result}).to_string();
            let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
            let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
            let attempt = AuthorizationAttempt::new(None).unwrap();
            let error = future::block_on(client.exchange_token(&attempt, "code")).unwrap_err();
            assert_eq!(error.kind(), &CloudErrorKind::Protocol);
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }

    #[test]
    fn token_errors_preserve_oauth_codes_without_response_text() {
        for (oauth_code, expected) in [
            (
                96002,
                "cloud business code -6 (OAuth error 96002: missing or invalid request parameters)",
            ),
            (
                96013,
                "cloud business code -6 (OAuth error 96013: invalid authorization code)",
            ),
            (99999, "cloud business code -6 (OAuth error 99999)"),
        ] {
            let message = json!({
                "error": oauth_code,
                "error_description": "response-secret\nhttps://example.invalid/?code=secret",
                "traceId": "trace-secret",
                "access_token": "access-secret",
            })
            .to_string();
            let body = json!({"code": -6, "message": message}).to_string();
            let (base, _) = mock_server(vec![
                MockResponse::json(200, &body),
                MockResponse::json(200, &body),
            ]);
            let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
            let attempt = AuthorizationAttempt::new(None).unwrap();
            let errors = [
                future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err(),
                future::block_on(client.refresh_token(
                    attempt.oauth_client_uuid(),
                    attempt.redirect_uri(),
                    "refresh-secret",
                ))
                .unwrap_err(),
            ];
            for (error, operation) in errors
                .into_iter()
                .zip(["exchange authorization code", "refresh access token"])
            {
                assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
                assert!(!error.is_unauthorized());
                assert_eq!(
                    error.to_string(),
                    format!("Failed to {operation}: {expected}")
                );
                assert!(!format!("{error:?} {error}").contains("secret"));
            }
        }
    }

    #[test]
    fn unusable_oauth_details_preserve_the_outer_business_error() {
        for message in [
            Value::Null,
            json!("response-secret"),
            json!({"error": 96013, "error_description": "response-secret"}),
            json!(r#"{"error_description":"response-secret"}"#),
            json!(r#"{"error":"96013","error_description":"response-secret"}"#),
            json!(r#"{"error":96013.5,"error_description":"response-secret"}"#),
            json!(r#"{"error":true,"error_description":"response-secret"}"#),
        ] {
            let body = json!({"code": -6, "message": message}).to_string();
            let (base, _) = mock_server(vec![MockResponse::json(200, &body)]);
            let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
            let attempt = AuthorizationAttempt::new(None).unwrap();
            let error =
                future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err();
            assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
            assert_eq!(
                error.to_string(),
                "Failed to exchange authorization code: cloud business code -6"
            );
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }

    #[test]
    fn oauth_details_do_not_override_http_or_protocol_errors() {
        let message = json!({"error": 96013, "error_description": "response-secret"}).to_string();
        for (status, body, expected) in [
            (
                401,
                json!({"code": -6, "message": message}),
                CloudErrorKind::Unauthorized,
            ),
            (
                403,
                json!({"code": -6, "message": message}),
                CloudErrorKind::HttpStatus(403),
            ),
            (200, json!({"message": message}), CloudErrorKind::Protocol),
            (
                200,
                json!({"code": 0, "message": message}),
                CloudErrorKind::Protocol,
            ),
        ] {
            let (base, _) = mock_server(vec![MockResponse::json(status, &body.to_string())]);
            let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
            let attempt = AuthorizationAttempt::new(None).unwrap();
            let error =
                future::block_on(client.exchange_token(&attempt, "code-secret")).unwrap_err();
            assert_eq!(error.kind(), &expected);
            assert!(!error.to_string().contains("OAuth"));
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }

    #[test]
    fn protected_requests_do_not_interpret_oauth_details() {
        let body = json!({
            "code": -6,
            "message": json!({"error": 96013, "error_description": "response-secret"}).to_string(),
        })
        .to_string();
        let (base, _) = mock_server(vec![
            MockResponse::json(200, &body),
            MockResponse::json(200, &body),
        ]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        for error in [
            future::block_on(client.get_home("access-secret")).unwrap_err(),
            future::block_on(client.get_certificate("access-secret", "csr")).unwrap_err(),
        ] {
            assert_eq!(error.kind(), &CloudErrorKind::Business(-6));
            assert!(!error.is_unauthorized());
            assert!(!error.to_string().contains("OAuth"));
            assert!(!format!("{error:?} {error}").contains("secret"));
        }
    }

    #[test]
    fn home_and_device_requests_are_minimal_and_bounded() {
        let dids = (0..160)
            .map(|index| format!("did-{index}"))
            .collect::<Vec<_>>();
        let home = json!({"code":0,"result":{"homelist":[{"uid":123,"dids":dids,"roomlist":[{"dids":["did-0","room-only"]}]}],"share_home_list":[{"uid":"shared"}],"has_more":true}});
        let (base, requests) = mock_server(vec![
            MockResponse::json(200, &home.to_string()),
            MockResponse::json(200, r#"{"code":0,"result":{"list":[],"has_more":true}}"#),
        ]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let page = future::block_on(client.get_home("access-secret")).unwrap();
        assert_eq!(page.uid.as_deref(), Some("123"));
        assert_eq!(page.dids.len(), 150);
        future::block_on(client.get_devices("access-secret", &page.dids)).unwrap();
        let home_request = requests.recv().unwrap();
        assert!(home_request.target.ends_with("/app/v2/homeroom/gethome"));
        assert!(
            home_request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: beareraccess-secret")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&home_request.body).unwrap(),
            json!({"limit":150,"fetch_share":false,"fetch_share_dev":false,"plat_form":0,"app_ver":9})
        );
        let device_request = requests.recv().unwrap();
        let body: Value = serde_json::from_str(&device_request.body).unwrap();
        assert_eq!(body["dids"].as_array().unwrap().len(), 150);
        assert_eq!(body["limit"], 200);
    }

    #[test]
    fn floating_point_uid_is_a_protocol_error() {
        let (base, _) = mock_server(vec![MockResponse::json(
            200,
            r#"{"code":0,"result":{"homelist":[{"uid":12.5}]}}"#,
        )]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        let error = future::block_on(client.get_home("access")).unwrap_err();
        assert_eq!(error.kind(), &CloudErrorKind::Protocol);
    }

    #[test]
    fn cloud_errors_are_classified_and_sanitized() {
        for (status, body, unauthorized) in [
            (401, "token-secret", true),
            (403, "forbidden-secret", false),
            (200, r#"{"code":-7,"message":"business-secret"}"#, false),
            (200, "protocol-secret", false),
        ] {
            let (base, _) = mock_server(vec![MockResponse::json(status, body)]);
            let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
            let error = future::block_on(client.get_home("access-secret")).unwrap_err();
            assert_eq!(error.is_unauthorized(), unauthorized);
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains("secret"));
        }
    }

    #[test]
    fn certificate_request_sends_only_base64_csr_and_empty_devices_skip_http() {
        let (base, requests) = mock_server(vec![MockResponse::json(
            200,
            r#"{"code":0,"result":{"cert":"certificate-pem"}}"#,
        )]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        future::block_on(client.get_devices("access", &[])).unwrap();
        assert_eq!(
            future::block_on(client.get_certificate("access", "csr-pem")).unwrap(),
            "certificate-pem"
        );
        let request = requests.recv().unwrap();
        assert!(request.target.ends_with("/app/v2/ha/oauth/get_central_crt"));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(
            STANDARD.decode(body["csr"].as_str().unwrap()).unwrap(),
            b"csr-pem"
        );
    }

    #[test]
    fn requests_time_out_and_do_not_follow_redirects() {
        let (base, _) = mock_server(vec![
            MockResponse::json(200, r#"{"code":0,"result":{"homelist":[]}}"#)
                .delayed(Duration::from_millis(100)),
        ]);
        let client = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
        assert!(
            future::block_on(client.get_home("access"))
                .unwrap_err()
                .is_timeout()
        );

        let (base, _) = mock_server(vec![MockResponse::delayed_body(
            200,
            r#"{"code":0,"result":{"homelist":[]}}"#,
            5,
            Duration::from_millis(100),
        )]);
        let client = CloudClient::for_test(&base, Duration::from_millis(10)).unwrap();
        assert!(
            future::block_on(client.get_home("access"))
                .unwrap_err()
                .is_timeout()
        );

        let redirect = "HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (base, _) = mock_server(vec![MockResponse::raw(redirect)]);
        let client = CloudClient::for_test(&base, Duration::from_secs(1)).unwrap();
        assert_eq!(
            future::block_on(client.get_home("access"))
                .unwrap_err()
                .http_status(),
            Some(302)
        );
    }
}
