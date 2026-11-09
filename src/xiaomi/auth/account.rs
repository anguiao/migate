mod cookies;
mod http;
mod oauth;

use crate::xiaomi::{cloud::AuthorizationAttempt, mijia::MijiaSession};
use http::{AccountFlow, account_json, endpoint};
use isahc::{cookies::Cookie, http::header::HeaderValue};
use serde_json::Value;
use std::{
    error::Error as StdError,
    fmt,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ACCOUNT_URL: &str = "https://account.xiaomi.com";
const STS_URL: &str = "https://sts.api.io.mi.com/sts";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountStage {
    QrRequest,
    QrPolling,
    ServiceLogin,
    Sts,
    HaAuthorization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountErrorKind {
    Rejected,
    VerificationRequired,
    Timeout,
    Network,
    Protocol,
    IdentityMismatch,
    InvalidInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountError {
    stage: AccountStage,
    kind: AccountErrorKind,
    detail: Option<&'static str>,
}

impl AccountError {
    fn new(stage: AccountStage, kind: AccountErrorKind) -> Self {
        Self {
            stage,
            kind,
            detail: None,
        }
    }
    fn protocol(stage: AccountStage) -> Self {
        Self::new(stage, AccountErrorKind::Protocol)
    }
    fn at(mut self, detail: &'static str) -> Self {
        if self.detail.is_none() {
            self.detail = Some(detail);
        }
        self
    }
    pub fn stage(&self) -> AccountStage {
        self.stage
    }
    pub fn kind(&self) -> AccountErrorKind {
        self.kind
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Xiaomi account {:?}: {:?}", self.stage, self.kind)?;
        if let Some(detail) = self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
    }
}
impl StdError for AccountError {}

/// Stable identity shared by account login and the device cloud session.
#[derive(Clone, Eq, PartialEq)]
pub struct AccountClientIdentity {
    device_id: String,
    user_agent: String,
}

impl AccountClientIdentity {
    pub fn new(device_id: String, user_agent: String) -> Result<Self, AccountError> {
        if device_id.is_empty()
            || Cookie::builder("deviceId", &device_id).build().is_err()
            || user_agent.trim().is_empty()
            || HeaderValue::from_str(&user_agent).is_err()
        {
            return Err(AccountError::new(
                AccountStage::ServiceLogin,
                AccountErrorKind::InvalidInput,
            ));
        }
        Ok(Self {
            device_id,
            user_agent,
        })
    }

    pub fn generate() -> Self {
        let device_id = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let user_agent = format!(
            "Android-7.1.1-1.0.0-ONEPLUS A3010-136-{device_id} APP/xiaomi.smarthome APPV/62830"
        );
        Self {
            device_id,
            user_agent,
        }
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}
impl fmt::Debug for AccountClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountClientIdentity")
            .field("device_id", &"[REDACTED]")
            .field("user_agent", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AccountCredentials {
    user_id: String,
    c_user_id: String,
    pass_token: String,
    identity: AccountClientIdentity,
}
impl AccountCredentials {
    pub fn new(
        user_id: String,
        c_user_id: String,
        pass_token: String,
        identity: AccountClientIdentity,
    ) -> Result<Self, AccountError> {
        if [
            ("userId", &user_id),
            ("cUserId", &c_user_id),
            ("passToken", &pass_token),
        ]
        .iter()
        .any(|(name, value)| value.is_empty() || Cookie::builder(*name, *value).build().is_err())
        {
            return Err(AccountError::new(
                AccountStage::ServiceLogin,
                AccountErrorKind::InvalidInput,
            ));
        }
        Ok(Self {
            user_id,
            c_user_id,
            pass_token,
            identity,
        })
    }
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn c_user_id(&self) -> &str {
        &self.c_user_id
    }
    pub fn pass_token(&self) -> &str {
        &self.pass_token
    }
    pub fn identity(&self) -> &AccountClientIdentity {
        &self.identity
    }
}
impl fmt::Debug for AccountCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountCredentials")
            .field("user_id", &"[REDACTED]")
            .field("c_user_id", &"[REDACTED]")
            .field("pass_token", &"[REDACTED]")
            .field("identity", &self.identity)
            .finish()
    }
}

#[derive(Clone)]
pub struct AccountClient {
    timeout: Duration,
    #[cfg(test)]
    test_port: Option<u16>,
}
impl Default for AccountClient {
    fn default() -> Self {
        Self::new()
    }
}
impl AccountClient {
    pub fn new() -> Self {
        Self {
            timeout: REQUEST_TIMEOUT,
            #[cfg(test)]
            test_port: None,
        }
    }

    #[cfg(test)]
    pub(in crate::xiaomi) fn for_test(base: &str, timeout: Duration) -> Self {
        Self {
            timeout,
            test_port: Some(Url::parse(base).unwrap().port().unwrap()),
        }
    }

    pub async fn start_qr(
        &self,
        identity: AccountClientIdentity,
    ) -> Result<QrAttempt, AccountError> {
        let stage = AccountStage::QrRequest;
        let flow = AccountFlow::new(self, identity, stage)?;
        let mut url = endpoint(ACCOUNT_URL, "/longPolling/loginUrl", stage)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AccountError::protocol(stage))?
            .as_millis();
        url.query_pairs_mut()
            .append_pair("_qrsize", "400")
            .append_pair("qs", "%3Fsid%3Dxiaomiio%26_json%3Dtrue")
            .append_pair("callback", STS_URL)
            .append_pair("_hasLogo", "false")
            .append_pair("sid", "xiaomiio")
            .append_pair("serviceParam", "")
            .append_pair("_locale", "zh_CN")
            .append_pair("_dc", &now.to_string());
        let requested_at = Instant::now();
        let response = flow.send_get(&url, self.timeout, stage).await?;
        let data = account_json(response, &url, stage).await?;
        check_response(&data, stage)?;
        let login_url = field(&data, "loginUrl", stage)?;
        let qr_url = field(&data, "qr", stage)?;
        let polling_url =
            Url::parse(&field(&data, "lp", stage)?).map_err(|_| AccountError::protocol(stage))?;
        http::validate_qr_metadata(&login_url, &qr_url, &polling_url, stage)?;
        let timeout = data
            .get("timeout")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
            .ok_or_else(|| AccountError::protocol(stage))?;
        let expires_at = requested_at
            .checked_add(Duration::from_secs(timeout))
            .ok_or_else(|| AccountError::protocol(stage))?;
        Ok(QrAttempt {
            flow,
            login_url,
            qr_url,
            polling_url,
            expires_at,
        })
    }

    pub async fn restore(
        &self,
        credentials: &AccountCredentials,
    ) -> Result<RestoredAccount, AccountError> {
        let stage = AccountStage::ServiceLogin;
        let flow = AccountFlow::new(self, credentials.identity.clone(), stage)?;
        flow.install_credentials(credentials, stage)?;
        let mut url = endpoint(ACCOUNT_URL, "/pass/serviceLogin", stage)?;
        url.query_pairs_mut()
            .append_pair("_json", "true")
            .append_pair("sid", "xiaomiio");
        let data =
            account_json(flow.send_get(&url, self.timeout, stage).await?, &url, stage).await?;
        check_response(&data, stage)?;
        if let Some(uid) = optional_field(&data, "userId", stage)?
            && uid != credentials.user_id
        {
            return Err(AccountError::new(stage, AccountErrorKind::IdentityMismatch));
        }
        let updated = AccountCredentials::new(
            credentials.user_id.clone(),
            optional_field(&data, "cUserId", stage)?
                .unwrap_or_else(|| credentials.c_user_id.clone()),
            optional_field(&data, "passToken", stage)?
                .unwrap_or_else(|| credentials.pass_token.clone()),
            credentials.identity.clone(),
        )
        .map_err(|_| AccountError::protocol(stage))?;
        let session = flow.exchange_sts(&data, &updated.user_id).await?;
        Ok(RestoredAccount {
            credentials: updated,
            session,
        })
    }
}

pub struct QrAttempt {
    flow: AccountFlow,
    login_url: String,
    qr_url: String,
    polling_url: Url,
    expires_at: Instant,
}
impl QrAttempt {
    pub fn login_url(&self) -> &str {
        &self.login_url
    }
    pub fn qr_url(&self) -> &str {
        &self.qr_url
    }
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    pub async fn poll(
        self,
        expected_uid: Option<&str>,
    ) -> Result<AuthenticatedAccount, AccountError> {
        let stage = AccountStage::QrPolling;
        let data = loop {
            let remaining = self.remaining();
            if remaining.is_zero() {
                return Err(AccountError::new(stage, AccountErrorKind::Timeout));
            }
            let response = self
                .flow
                .send_get(&self.polling_url, remaining.min(self.flow.timeout), stage)
                .await;
            match response {
                Err(error) if error.kind == AccountErrorKind::Timeout => continue,
                Err(error) => return Err(error),
                Ok(response) => break account_json(response, &self.polling_url, stage).await?,
            }
        };
        if self.remaining().is_zero() {
            return Err(AccountError::new(stage, AccountErrorKind::Timeout));
        }
        check_response(&data, stage)?;
        let user_id = field(&data, "userId", stage)?;
        if expected_uid.is_some_and(|expected| expected != user_id) {
            return Err(AccountError::new(stage, AccountErrorKind::IdentityMismatch));
        }
        let credentials = AccountCredentials::new(
            user_id,
            field(&data, "cUserId", stage)?,
            field(&data, "passToken", stage)?,
            self.flow.identity.clone(),
        )
        .map_err(|_| AccountError::protocol(stage))?;
        self.flow.install_credentials(&credentials, stage)?;
        let session = self.flow.exchange_sts(&data, credentials.user_id()).await?;
        Ok(AuthenticatedAccount {
            flow: self.flow,
            credentials,
            session,
        })
    }
}
impl fmt::Debug for QrAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrAttempt")
            .field("login_url", &"[REDACTED]")
            .field("qr_url", &"[REDACTED]")
            .field("polling_url", &"[REDACTED]")
            .field("remaining", &self.remaining())
            .finish()
    }
}

pub struct AuthenticatedAccount {
    flow: AccountFlow,
    credentials: AccountCredentials,
    session: MijiaSession,
}
impl AuthenticatedAccount {
    pub fn credentials(&self) -> &AccountCredentials {
        &self.credentials
    }
    pub fn session(&self) -> &MijiaSession {
        &self.session
    }
    pub async fn authorize(
        self,
        attempt: &AuthorizationAttempt,
    ) -> Result<AccountGrant, AccountError> {
        let code = self
            .flow
            .authorize(attempt, self.credentials.user_id())
            .await?;
        Ok(AccountGrant {
            credentials: self.credentials,
            session: self.session,
            code,
        })
    }
}
impl fmt::Debug for AuthenticatedAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticatedAccount")
            .field("credentials", &self.credentials)
            .field("session", &"[REDACTED]")
            .finish()
    }
}

pub struct AccountGrant {
    credentials: AccountCredentials,
    session: MijiaSession,
    code: String,
}
impl AccountGrant {
    pub fn credentials(&self) -> &AccountCredentials {
        &self.credentials
    }
    pub fn session(&self) -> &MijiaSession {
        &self.session
    }
    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn into_parts(self) -> (AccountCredentials, MijiaSession, String) {
        (self.credentials, self.session, self.code)
    }
}
impl fmt::Debug for AccountGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountGrant")
            .field("credentials", &self.credentials)
            .field("session", &"[REDACTED]")
            .field("code", &"[REDACTED]")
            .finish()
    }
}

pub struct RestoredAccount {
    credentials: AccountCredentials,
    session: MijiaSession,
}
impl RestoredAccount {
    pub fn credentials(&self) -> &AccountCredentials {
        &self.credentials
    }
    pub fn session(&self) -> &MijiaSession {
        &self.session
    }
    pub fn into_parts(self) -> (AccountCredentials, MijiaSession) {
        (self.credentials, self.session)
    }
}
impl fmt::Debug for RestoredAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoredAccount")
            .field("credentials", &self.credentials)
            .field("session", &"[REDACTED]")
            .finish()
    }
}

fn optional_field(
    data: &Value,
    name: &str,
    stage: AccountStage,
) -> Result<Option<String>, AccountError> {
    match data.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(Value::Number(value)) if value.as_u64().is_some() => Ok(Some(value.to_string())),
        _ => Err(AccountError::protocol(stage)),
    }
}
fn field(data: &Value, name: &str, stage: AccountStage) -> Result<String, AccountError> {
    optional_field(data, name, stage)?.ok_or_else(|| AccountError::protocol(stage))
}
fn check_response(data: &Value, stage: AccountStage) -> Result<(), AccountError> {
    for key in [
        "notificationUrl",
        "captchaUrl",
        "verificationUrl",
        "verifyUrl",
    ] {
        if data
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        {
            return Err(AccountError::new(
                stage,
                AccountErrorKind::VerificationRequired,
            ));
        }
    }
    if let Some(code) = data.get("code") {
        let code = code.as_i64().ok_or_else(|| AccountError::protocol(stage))?;
        if code != 0 {
            return Err(AccountError::new(
                stage,
                match code {
                    70016 | 70002 | 401 | 403 | 96012 => AccountErrorKind::Rejected,
                    87001 | 87002 => AccountErrorKind::VerificationRequired,
                    _ => AccountErrorKind::Protocol,
                },
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
