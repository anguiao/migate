use super::{AuthorizationAttempt, CLIENT_ID, CloudError, CloudErrorKind};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, error::Error as StdError, fmt, time::Duration};
use url::Url;
use uuid::Uuid;

pub const CLOUD_BASE_URL: &str = "https://ha.api.io.mi.com";

#[derive(Clone, Eq, PartialEq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
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
                "client_id": CLIENT_ID,
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
                "client_id": CLIENT_ID,
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
        let result = result_object(&value, operation)
            .map_err(|error| error.with_oauth_code(oauth_error_code(&value)))?;
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
            .header("X-Client-AppId", CLIENT_ID.to_string())
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
