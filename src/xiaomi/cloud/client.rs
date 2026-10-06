use super::{
    AuthorizationAttempt, CLIENT_ID, CloudError, CloudErrorKind,
    response::{ApiResponse, Identifier, Members},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use isahc::{
    AsyncReadResponseExt, HttpClient, Request,
    config::{Configurable as _, RedirectPolicy},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, error::Error as StdError, fmt, time::Duration};
use url::Url;

pub const CLOUD_BASE_URL: &str = "https://ha.api.io.mi.com";
const PROBE_DEVICE_LIMIT: usize = 150;

#[derive(Clone, Deserialize, Eq, PartialEq)]
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

#[derive(Clone)]
pub struct CloudClient {
    client: HttpClient,
    base_url: String,
    timeout: Duration,
    pub(super) instance_base_url: String,
}

impl CloudClient {
    pub fn new() -> Result<Self, CloudError> {
        Self::build(CLOUD_BASE_URL, Duration::from_secs(30))
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: &str, timeout: Duration) -> Result<Self, CloudError> {
        let mut client = Self::build(base_url, timeout)?;
        client.instance_base_url = base_url.trim_end_matches('/').to_owned();
        Ok(client)
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
            timeout,
            instance_base_url: "https://miot-spec.org".to_owned(),
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
                "device_id": attempt.device_id(),
            }),
        )
        .await
    }

    pub async fn refresh_token(
        &self,
        redirect_uri: &str,
        refresh_token: &str,
    ) -> Result<TokenResponse, CloudError> {
        if redirect_uri.is_empty() || refresh_token.is_empty() {
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
        let response: ApiResponse = self.send(request, operation).await?;
        let oauth_code = response.oauth_error_code();
        let result: TokenResponse = response
            .into_result(operation)
            .map_err(|error| error.with_oauth_code(oauth_code))?;
        validate_token(&result.access_token, operation)?;
        validate_token(&result.refresh_token, operation)?;
        if result.expires_in <= 0 {
            return Err(CloudError::protocol(operation));
        }
        Ok(result)
    }

    pub async fn get_home(&self, access_token: &str) -> Result<HomePage, CloudError> {
        #[derive(Deserialize)]
        struct ProbeHomes {
            homelist: Vec<ProbeHome>,
        }
        #[derive(Deserialize)]
        struct ProbeHome {
            uid: Option<Identifier>,
            #[serde(flatten)]
            members: Members,
        }
        let operation = "read Xiaomi homes";
        let result: ProbeHomes = self.protected_post(operation, "/app/v2/homeroom/gethome", access_token, json!({
            "limit": 150, "fetch_share": false, "fetch_share_dev": false, "plat_form": 0, "app_ver": 9
        })).await?;
        let mut uid = None;
        let mut dids = Vec::new();
        let mut seen = HashSet::new();
        for home in result.homelist {
            if uid.is_none() {
                uid = home
                    .uid
                    .map(Identifier::into_string)
                    .filter(|uid| !uid.is_empty());
            }
            for did in home.members.into_dids() {
                if dids.len() < PROBE_DEVICE_LIMIT && seen.insert(did.clone()) {
                    dids.push(did);
                }
            }
        }
        Ok(HomePage { uid, dids })
    }

    pub async fn get_devices(&self, access_token: &str, dids: &[String]) -> Result<(), CloudError> {
        #[derive(Deserialize)]
        struct DeviceProbe {
            #[serde(rename = "list")]
            _devices: Vec<Map<String, Value>>,
        }
        if dids.is_empty() {
            return Ok(());
        }
        self.protected_post::<DeviceProbe>("read Xiaomi devices", "/app/v2/home/device_list_page", access_token, json!({
            "limit": 200, "get_split_device": true, "get_third_device": true, "dids": &dids[..dids.len().min(PROBE_DEVICE_LIMIT)]
        })).await?;
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
        #[derive(Deserialize)]
        struct Certificate {
            cert: String,
        }
        let result: Certificate = self
            .protected_post(
                operation,
                "/app/v2/ha/oauth/get_central_crt",
                access_token,
                json!({
                    "csr": STANDARD.encode(csr_pem.as_bytes())
                }),
            )
            .await?;
        if result.cert.is_empty() {
            return Err(CloudError::protocol(operation));
        }
        Ok(result.cert)
    }

    pub(super) async fn protected_post<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        access_token: &str,
        body: Value,
    ) -> Result<T, CloudError> {
        self.protected_post_with_timeout(operation, path, access_token, body, self.timeout)
            .await
    }

    pub(super) async fn protected_post_with_timeout<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        access_token: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T, CloudError> {
        if access_token.is_empty() {
            return Err(CloudError::input(operation));
        }
        let url = self.endpoint(path, operation)?;
        let request = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .timeout(timeout.min(self.timeout))
            .header("Content-Type", "application/json")
            .header("X-Client-BizId", "haapi")
            .header("X-Client-AppId", CLIENT_ID.to_string())
            .header("Authorization", format!("Bearer{access_token}"))
            .body(body.to_string())
            .map_err(|_| CloudError::protocol(operation))?;
        self.send::<ApiResponse>(request, operation)
            .await?
            .into_result(operation)
    }

    fn endpoint(&self, path: &str, operation: &'static str) -> Result<Url, CloudError> {
        Url::parse(&format!("{}{path}", self.base_url)).map_err(|_| CloudError::protocol(operation))
    }

    pub(super) async fn send<T: DeserializeOwned>(
        &self,
        request: Request<String>,
        operation: &'static str,
    ) -> Result<T, CloudError> {
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

fn validate_token(value: &str, operation: &'static str) -> Result<(), CloudError> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(CloudError::protocol(operation));
    }
    Ok(())
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
