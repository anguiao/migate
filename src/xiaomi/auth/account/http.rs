use super::{
    ACCOUNT_URL, AccountClient, AccountClientIdentity, AccountCredentials, AccountError,
    AccountErrorKind, AccountStage, MijiaSession, check_response, cookies::remove_expired_cookies,
    field,
};
use crate::xiaomi::{cloud::CloudErrorKind, http::response_read_error_kind};
use isahc::{
    AsyncBody, AsyncReadResponseExt, HttpClient, Request, Response,
    config::{Configurable as _, RedirectPolicy},
    cookies::{Cookie, CookieJar},
    http::Uri,
};
use serde_json::Value;
use std::time::Duration;
use url::Url;

const MAX_REDIRECTS: usize = 8;

pub(super) struct AccountFlow {
    client: HttpClient,
    jar: CookieJar,
    pub(super) identity: AccountClientIdentity,
    pub(super) timeout: Duration,
}

impl AccountFlow {
    pub(super) fn new(
        config: &AccountClient,
        identity: AccountClientIdentity,
        stage: AccountStage,
    ) -> Result<Self, AccountError> {
        let jar = CookieJar::new();
        let client = (config.client_builder)()
            .timeout(config.timeout)
            .redirect_policy(RedirectPolicy::None)
            .cookie_jar(jar.clone())
            .build()
            .map_err(|_| AccountError::new(stage, AccountErrorKind::Network))?;
        let flow = Self {
            client,
            jar,
            identity,
            timeout: config.timeout,
        };
        flow.set_cookie("deviceId", flow.identity.device_id(), true, stage)?;
        Ok(flow)
    }

    pub(super) fn install_credentials(
        &self,
        credentials: &AccountCredentials,
        stage: AccountStage,
    ) -> Result<(), AccountError> {
        self.set_cookie("userId", credentials.user_id(), false, stage)?;
        self.set_cookie("cUserId", credentials.c_user_id(), false, stage)?;
        self.set_cookie("passToken", credentials.pass_token(), false, stage)
    }

    fn set_cookie(
        &self,
        name: &str,
        value: &str,
        subdomains: bool,
        stage: AccountStage,
    ) -> Result<(), AccountError> {
        let builder = Cookie::builder(name, value).path("/");
        let builder = if subdomains {
            builder.domain("account.xiaomi.com")
        } else {
            builder
        };
        let cookie = builder
            .secure(true)
            .build()
            .map_err(|_| AccountError::protocol(stage))?;
        let uri: Uri = ACCOUNT_URL
            .parse()
            .map_err(|_| AccountError::protocol(stage))?;
        self.jar
            .set(cookie, &uri)
            .map_err(|_| AccountError::protocol(stage))?;
        Ok(())
    }

    pub(super) async fn send_get(
        &self,
        url: &Url,
        timeout: Duration,
        stage: AccountStage,
    ) -> Result<Response<AsyncBody>, AccountError> {
        self.send(url, "GET", None, timeout, stage).await
    }

    pub(super) async fn send_post(
        &self,
        url: &Url,
        body: String,
        stage: AccountStage,
    ) -> Result<Response<AsyncBody>, AccountError> {
        self.send(url, "POST", Some(body), self.timeout, stage)
            .await
    }

    async fn send(
        &self,
        url: &Url,
        method: &str,
        body: Option<String>,
        timeout: Duration,
        stage: AccountStage,
    ) -> Result<Response<AsyncBody>, AccountError> {
        validate_request_url(url, stage)?;
        let mut request = Request::builder()
            .method(method)
            .uri(url.as_str())
            .timeout(timeout)
            .header("User-Agent", self.identity.user_agent());
        if body.is_some() {
            request = request.header("Content-Type", "application/x-www-form-urlencoded");
        }
        let request = request
            .body(body.unwrap_or_default())
            .map_err(|_| AccountError::protocol(stage))?;
        let response = self.client.send_async(request).await.map_err(|error| {
            AccountError::new(
                stage,
                if error.is_timeout() {
                    AccountErrorKind::Timeout
                } else {
                    AccountErrorKind::Network
                },
            )
        })?;
        let uri: Uri = url
            .as_str()
            .parse()
            .map_err(|_| AccountError::protocol(stage))?;
        remove_expired_cookies(
            &self.jar,
            &uri,
            response.headers(),
            std::time::SystemTime::now(),
        );
        Ok(response)
    }

    pub(super) async fn exchange_sts(
        &self,
        data: &Value,
        user_id: &str,
    ) -> Result<MijiaSession, AccountError> {
        let stage = AccountStage::Sts;
        let ssecurity = field(data, "ssecurity", stage)?;
        let mut url = Url::parse(&field(data, "location", stage)?)
            .map_err(|_| AccountError::protocol(stage))?;
        for _ in 0..=MAX_REDIRECTS {
            validate_sts_url(&url, stage)?;
            let mut response = self.send_get(&url, self.timeout, stage).await?;
            let next = if response.status().is_redirection() {
                let target = redirect_url(&url, &response, stage)?;
                validate_sts_url(&target, stage)?;
                Some(target)
            } else {
                status(response.status().as_u16(), stage)?;
                let body = response
                    .bytes()
                    .await
                    .map_err(|error| read_error(&error, stage))?;
                let body = body.strip_prefix(b"&&&START&&&").unwrap_or(&body);
                let first = body
                    .iter()
                    .find(|&&byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'));
                if first == Some(&b'{') {
                    let data =
                        serde_json::from_slice(body).map_err(|_| AccountError::protocol(stage))?;
                    check_response(&data, stage)?;
                }
                None
            };
            let uri: Uri = url
                .as_str()
                .parse()
                .map_err(|_| AccountError::protocol(stage))?;
            if let Some(cookie) = self.jar.get_by_name(&uri, "serviceToken") {
                return MijiaSession::new(
                    user_id.to_owned(),
                    cookie.value().to_owned(),
                    ssecurity,
                    self.identity.user_agent().to_owned(),
                )
                .map_err(|_| AccountError::protocol(stage));
            }
            if let Some(target) = next {
                url = target;
            } else {
                return Err(AccountError::protocol(stage));
            }
        }
        Err(AccountError::protocol(stage))
    }
}

pub(super) fn endpoint(base: &str, path: &str, stage: AccountStage) -> Result<Url, AccountError> {
    Url::parse(base)
        .and_then(|url| url.join(path))
        .map_err(|_| AccountError::protocol(stage))
}

fn validate_common(url: &Url, stage: AccountStage) -> Result<(), AccountError> {
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AccountError::protocol(stage));
    }
    Ok(())
}
fn account_host(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("account.xiaomi.com" | "c3.account.xiaomi.com" | "c3.lp.account.xiaomi.com")
    )
}
pub(super) fn validate_qr_metadata(
    login_url: &str,
    qr_url: &str,
    polling: &Url,
    stage: AccountStage,
) -> Result<(), AccountError> {
    for value in [login_url, qr_url] {
        let url = Url::parse(value).map_err(|_| AccountError::protocol(stage))?;
        validate_common(&url, stage)?;
        if !account_host(&url) {
            return Err(AccountError::protocol(stage));
        }
    }
    validate_request_url(polling, AccountStage::QrPolling)
        .map_err(|error| AccountError::new(stage, error.kind()))
}
pub(super) fn validate_sts_url(url: &Url, stage: AccountStage) -> Result<(), AccountError> {
    validate_common(url, stage)?;
    if account_host(url) && is_verification_path(url.path()) {
        return Err(AccountError::new(
            stage,
            AccountErrorKind::VerificationRequired,
        ));
    }
    if url.host_str() != Some("sts.api.io.mi.com") || url.path() != "/sts" {
        return Err(AccountError::protocol(stage));
    }
    Ok(())
}
pub(super) fn validate_request_url(url: &Url, stage: AccountStage) -> Result<(), AccountError> {
    validate_common(url, stage)?;
    if account_host(url) && is_verification_path(url.path()) {
        return Err(AccountError::new(
            stage,
            AccountErrorKind::VerificationRequired,
        ));
    }
    let valid = match stage {
        AccountStage::QrRequest => {
            url.host_str() == Some("account.xiaomi.com") && url.path() == "/longPolling/loginUrl"
        }
        AccountStage::QrPolling => {
            (account_host(url) && url.path() == "/longPolling/login")
                || (url.host_str() == Some("c3.lp.account.xiaomi.com") && url.path() == "/lp/s")
        }
        AccountStage::ServiceLogin => {
            url.host_str() == Some("account.xiaomi.com") && url.path() == "/pass/serviceLogin"
        }
        AccountStage::Sts => return validate_sts_url(url, stage),
        AccountStage::HaAuthorization => {
            url.host_str() == Some("account.xiaomi.com")
                && matches!(
                    url.path(),
                    "/oauth2/authorize"
                        | "/oauth2/userAuthorization"
                        | "/pass/serviceLogin"
                        | "/sts"
                        | "/sts/oauth"
                        | "/fe/service/oauth2/authorize"
                )
        }
    };
    if valid {
        Ok(())
    } else {
        Err(AccountError::protocol(stage))
    }
}

fn is_verification_path(path: &str) -> bool {
    path.starts_with("/pass/notification")
        || path.starts_with("/pass/verify")
        || path.contains("captcha")
}

pub(super) fn redirect_url(
    base: &Url,
    response: &Response<AsyncBody>,
    stage: AccountStage,
) -> Result<Url, AccountError> {
    let value = response
        .headers()
        .get(isahc::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AccountError::protocol(stage))?;
    base.join(value).map_err(|_| AccountError::protocol(stage))
}

pub(super) fn status(status: u16, stage: AccountStage) -> Result<(), AccountError> {
    match status {
        200 => Ok(()),
        401 | 403 => Err(AccountError::new(stage, AccountErrorKind::Rejected)),
        408 => Err(AccountError::new(stage, AccountErrorKind::Timeout)),
        429 | 500..=599 => Err(AccountError::new(stage, AccountErrorKind::Network)),
        _ => Err(AccountError::protocol(stage)),
    }
}

pub(super) async fn response_json(
    mut response: Response<AsyncBody>,
    stage: AccountStage,
) -> Result<Value, AccountError> {
    status(response.status().as_u16(), stage)?;
    let bytes = response
        .bytes()
        .await
        .map_err(|error| read_error(&error, stage))?;
    let bytes = bytes.strip_prefix(b"&&&START&&&").unwrap_or(&bytes);
    serde_json::from_slice(bytes).map_err(|_| AccountError::protocol(stage))
}

pub(super) async fn account_json(
    response: Response<AsyncBody>,
    url: &Url,
    stage: AccountStage,
) -> Result<Value, AccountError> {
    if response.status().is_redirection() {
        let target = redirect_url(url, &response, stage)?;
        validate_request_url(&target, stage)?;
        return Err(AccountError::protocol(stage));
    }
    response_json(response, stage).await
}

fn read_error(error: &std::io::Error, stage: AccountStage) -> AccountError {
    AccountError::new(
        stage,
        match response_read_error_kind(error) {
            CloudErrorKind::Timeout => AccountErrorKind::Timeout,
            _ => AccountErrorKind::Network,
        },
    )
}
