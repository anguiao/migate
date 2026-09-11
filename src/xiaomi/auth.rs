use crate::{
    storage::{StorageError, TokenSet, XiaomiRecord, XiaomiStore},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity, validate_certificate},
        cloud::{
            AuthorizationAttempt, CloudClient, CloudError, REGION, validate_saved_redirect_uri,
        },
    },
};
use std::{error::Error as StdError, fmt};
use time::OffsetDateTime;

pub struct AuthService {
    store: XiaomiStore,
    cloud: CloudClient,
    now: fn() -> i64,
}

impl AuthService {
    pub fn new(store: XiaomiStore, cloud: CloudClient) -> Self {
        Self {
            store,
            cloud,
            now: current_time,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(store: XiaomiStore, cloud: CloudClient, now: fn() -> i64) -> Self {
        Self { store, cloud, now }
    }

    pub fn begin_login(&self) -> Result<LoginAttempt, AuthError> {
        let previous = self.load_validated()?;
        let authorization = AuthorizationAttempt::new(
            previous
                .as_ref()
                .map(|candidate| candidate.record.oauth_client_uuid.as_str()),
        )
        .map_err(AuthError::Cloud)?;
        Ok(LoginAttempt {
            previous,
            authorization,
        })
    }

    pub fn local_status(&self) -> Result<AuthReport, AuthError> {
        let status = self.load_validated()?;
        Ok(match status {
            Some(stored) => AuthReport {
                authentication: AuthenticationState::Checking,
                certificate: Some(stored.validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            },
            None => AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            },
        })
    }

    pub async fn complete_login(
        &self,
        attempt: LoginAttempt,
        callback: &str,
    ) -> Result<AuthReport, AuthError> {
        let previous_certificate = attempt.previous.as_ref().map(|value| value.validity);
        let code = match attempt.authorization.parse_callback(callback) {
            Ok(code) => code,
            Err(error) => {
                return Ok(AuthReport::failed_cloud_auth(
                    error,
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let response = match self
            .cloud
            .exchange_token(&attempt.authorization, &code)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Ok(AuthReport::failed_cloud_auth(
                    error,
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let tokens = match response.to_token_set((self.now)()) {
            Ok(tokens) => tokens,
            Err(error) => {
                return Ok(AuthReport::failed_auth(
                    FailureReason::Cloud(error),
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let home = match self.cloud.get_home(&tokens.access_token).await {
            Ok(home) => home,
            Err(error) => {
                return Ok(AuthReport::failed_cloud_auth(
                    error,
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        if let Err(error) = self
            .cloud
            .get_devices(&tokens.access_token, &home.dids)
            .await
        {
            return Ok(AuthReport::failed_cloud_auth(
                error,
                previous_certificate,
                (self.now)(),
            ));
        }
        let Some(uid) = home.uid else {
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: previous_certificate,
                certificate_update: CertificateUpdate::Failed(
                    FailureReason::CannotDetermineAccountIdentity,
                ),
                completed_at: (self.now)(),
            });
        };

        let certificate_decision_time = (self.now)();
        let same_account = attempt
            .previous
            .as_ref()
            .filter(|previous| previous.record.uid == uid);
        if let Some(previous) = same_account
            && previous.validity.currently_valid(certificate_decision_time)
            && !previous.validity.renewal_due(certificate_decision_time)
        {
            let mut record = previous.record.clone();
            record.oauth_client_uuid = attempt.authorization.oauth_client_uuid().to_owned();
            record.redirect_uri = attempt.authorization.redirect_uri().to_owned();
            record.tokens = tokens;
            self.store.replace(&record).map_err(AuthError::Storage)?;
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: Some(previous.validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        }

        let identity = match same_account {
            Some(previous) => ClientIdentity::from_private_key(
                &uid,
                &previous.record.virtual_did,
                &previous.record.private_key_pem,
            ),
            None => ClientIdentity::generate(&uid),
        };
        let identity = match identity {
            Ok(identity) => identity,
            Err(_) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::InvalidClientIdentity,
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let certificate_pem = match self
            .cloud
            .get_certificate(&tokens.access_token, &identity.csr_pem)
            .await
        {
            Ok(certificate) => certificate,
            Err(error) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::Cloud(error),
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let validity = match validate_certificate(
            &uid,
            &identity.virtual_did,
            &identity.private_key_pem,
            &certificate_pem,
        ) {
            Ok(validity) => validity,
            Err(_) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::InvalidCertificate,
                    previous_certificate,
                    (self.now)(),
                ));
            }
        };
        let completed_at = (self.now)();
        if !validity.currently_valid(completed_at) {
            return Ok(AuthReport::failed_certificate(
                FailureReason::CertificateNotCurrentlyValid,
                previous_certificate,
                completed_at,
            ));
        }
        let record = XiaomiRecord {
            uid,
            region: REGION.to_owned(),
            oauth_client_uuid: attempt.authorization.oauth_client_uuid().to_owned(),
            redirect_uri: attempt.authorization.redirect_uri().to_owned(),
            tokens,
            virtual_did: identity.virtual_did,
            private_key_pem: identity.private_key_pem,
            certificate_pem,
        };
        self.store.replace(&record).map_err(AuthError::Storage)?;
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }

    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        let Some(stored) = self.load_validated()? else {
            return Ok(AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        };
        let record = stored.record;
        let old_validity = stored.validity;
        let mut tokens = record.tokens.clone();
        let mut refreshed = false;

        if (self.now)() >= tokens.refresh_at
            && let Err(failure) = self
                .refresh_once(&record, &mut tokens, &mut refreshed)
                .await
        {
            return self.request_failure_report(failure, old_validity);
        }

        let home = match self.cloud.get_home(&tokens.access_token).await {
            Ok(home) => home,
            Err(error) if error.is_unauthorized() && !refreshed => {
                if let Err(failure) = self
                    .refresh_once(&record, &mut tokens, &mut refreshed)
                    .await
                {
                    return self.request_failure_report(failure, old_validity);
                }
                match self.cloud.get_home(&tokens.access_token).await {
                    Ok(home) => home,
                    Err(error) => {
                        return Ok(self.protected_failure_report(error, old_validity, true));
                    }
                }
            }
            Err(error) => {
                return Ok(self.protected_failure_report(error, old_validity, refreshed));
            }
        };

        if let Err(error) = self
            .cloud
            .get_devices(&tokens.access_token, &home.dids)
            .await
        {
            if error.is_unauthorized() && !refreshed {
                if let Err(failure) = self
                    .refresh_once(&record, &mut tokens, &mut refreshed)
                    .await
                {
                    return self.request_failure_report(failure, old_validity);
                }
                if let Err(error) = self
                    .cloud
                    .get_devices(&tokens.access_token, &home.dids)
                    .await
                {
                    return Ok(self.protected_failure_report(error, old_validity, true));
                }
            } else {
                return Ok(self.protected_failure_report(error, old_validity, refreshed));
            }
        }

        let Some(uid) = home.uid else {
            return Ok(AuthReport::failed_auth(
                FailureReason::CannotDetermineAccountIdentity,
                Some(old_validity),
                (self.now)(),
            ));
        };
        if uid != record.uid {
            return Ok(AuthReport::failed_auth(
                FailureReason::AccountIdentityMismatch,
                Some(old_validity),
                (self.now)(),
            ));
        }

        let renewal_time = (self.now)();
        if !old_validity.renewal_due(renewal_time) {
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: Some(old_validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        }

        let identity = ClientIdentity::from_private_key(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
        )
        .map_err(|error| self.corrupt_record(error))?;
        let mut certificate = self
            .cloud
            .get_certificate(&tokens.access_token, &identity.csr_pem)
            .await;
        if certificate.as_ref().is_err_and(CloudError::is_unauthorized) {
            if refreshed {
                let error = certificate.unwrap_err();
                return Ok(self.certificate_protected_failure_report(error, old_validity, true));
            }
            if let Err(failure) = self
                .refresh_once(&record, &mut tokens, &mut refreshed)
                .await
            {
                return self.certificate_request_failure_report(failure, old_validity);
            }
            certificate = self
                .cloud
                .get_certificate(&tokens.access_token, &identity.csr_pem)
                .await;
            if certificate.as_ref().is_err_and(CloudError::is_unauthorized) {
                return Ok(self.certificate_protected_failure_report(
                    certificate.unwrap_err(),
                    old_validity,
                    true,
                ));
            }
        }
        let certificate_pem = match certificate {
            Ok(certificate) => certificate,
            Err(error) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::Cloud(error),
                    Some(old_validity),
                    (self.now)(),
                ));
            }
        };
        let new_validity = match validate_certificate(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
            &certificate_pem,
        ) {
            Ok(validity) => validity,
            Err(_) => {
                return Ok(AuthReport::failed_certificate(
                    FailureReason::InvalidCertificate,
                    Some(old_validity),
                    (self.now)(),
                ));
            }
        };
        let completed_at = (self.now)();
        if !new_validity.currently_valid(completed_at) {
            return Ok(AuthReport::failed_certificate(
                FailureReason::CertificateNotCurrentlyValid,
                Some(old_validity),
                completed_at,
            ));
        }
        self.store
            .update_certificate(&certificate_pem)
            .map_err(AuthError::Storage)?;
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(new_validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }

    pub fn logout(&self) -> Result<(), StorageError> {
        self.store.logout()
    }

    async fn refresh_once(
        &self,
        record: &XiaomiRecord,
        tokens: &mut TokenSet,
        refreshed: &mut bool,
    ) -> Result<(), RequestFailure> {
        *refreshed = true;
        let response = self
            .cloud
            .refresh_token(
                &record.oauth_client_uuid,
                &record.redirect_uri,
                &tokens.refresh_token,
            )
            .await
            .map_err(|error| {
                if error.is_unauthorized() {
                    RequestFailure::SignInRequired(FailureReason::Cloud(error))
                } else {
                    RequestFailure::Unavailable(FailureReason::Cloud(error))
                }
            })?;
        let replacement = response
            .to_token_set((self.now)())
            .map_err(|error| RequestFailure::Unavailable(FailureReason::Cloud(error)))?;
        self.store
            .update_tokens(&replacement)
            .map_err(RequestFailure::Storage)?;
        *tokens = replacement;
        Ok(())
    }

    fn load_validated(&self) -> Result<Option<StoredRecord>, AuthError> {
        let Some(record) = self.store.load().map_err(AuthError::Storage)? else {
            return Ok(None);
        };
        validate_saved_redirect_uri(&record.redirect_uri)
            .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?;
        let validity = validate_certificate(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
            &record.certificate_pem,
        )
        .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?;
        Ok(Some(StoredRecord { record, validity }))
    }

    fn corrupt_record(&self, source: impl Into<Box<dyn StdError>>) -> StorageError {
        StorageError::new(
            self.store.path(),
            "validate Xiaomi authentication credentials",
            source,
        )
    }

    fn request_failure_report(
        &self,
        failure: RequestFailure,
        validity: CertificateValidity,
    ) -> Result<AuthReport, AuthError> {
        match failure {
            RequestFailure::SignInRequired(reason) => Ok(AuthReport {
                authentication: AuthenticationState::SignInRequired(reason),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            }),
            RequestFailure::Unavailable(reason) => Ok(AuthReport::failed_auth(
                reason,
                Some(validity),
                (self.now)(),
            )),
            RequestFailure::Storage(error) => Err(AuthError::Storage(error)),
        }
    }

    fn protected_failure_report(
        &self,
        error: CloudError,
        validity: CertificateValidity,
        refreshed: bool,
    ) -> AuthReport {
        let authentication = if error.is_unauthorized() && refreshed {
            AuthenticationState::SignInRequired(FailureReason::Cloud(error))
        } else {
            AuthenticationState::Unavailable(FailureReason::Cloud(error))
        };
        AuthReport {
            authentication,
            certificate: Some(validity),
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at: (self.now)(),
        }
    }

    fn certificate_request_failure_report(
        &self,
        failure: RequestFailure,
        validity: CertificateValidity,
    ) -> Result<AuthReport, AuthError> {
        match failure {
            RequestFailure::SignInRequired(reason) => Ok(AuthReport {
                authentication: AuthenticationState::SignInRequired(reason.clone()),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::Failed(reason),
                completed_at: (self.now)(),
            }),
            RequestFailure::Unavailable(reason) => Ok(AuthReport {
                authentication: AuthenticationState::Unavailable(reason.clone()),
                certificate: Some(validity),
                certificate_update: CertificateUpdate::Failed(reason),
                completed_at: (self.now)(),
            }),
            RequestFailure::Storage(error) => Err(AuthError::Storage(error)),
        }
    }

    fn certificate_protected_failure_report(
        &self,
        error: CloudError,
        validity: CertificateValidity,
        refreshed: bool,
    ) -> AuthReport {
        let sign_in_required = error.is_unauthorized() && refreshed;
        let reason = FailureReason::Cloud(error);
        let authentication = if sign_in_required {
            AuthenticationState::SignInRequired(reason.clone())
        } else {
            AuthenticationState::Unavailable(reason.clone())
        };
        AuthReport {
            authentication,
            certificate: Some(validity),
            certificate_update: CertificateUpdate::Failed(reason),
            completed_at: (self.now)(),
        }
    }
}

fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

struct StoredRecord {
    record: XiaomiRecord,
    validity: CertificateValidity,
}

pub struct LoginAttempt {
    previous: Option<StoredRecord>,
    authorization: AuthorizationAttempt,
}

impl LoginAttempt {
    pub fn authorization(&self) -> &AuthorizationAttempt {
        &self.authorization
    }
}

impl fmt::Debug for LoginAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginAttempt")
            .field("has_previous", &self.previous.is_some())
            .field("authorization", &self.authorization)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticationState {
    NotSignedIn,
    Checking,
    Authenticated,
    SignInRequired(FailureReason),
    Unavailable(FailureReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CertificateUpdate {
    NotNeeded,
    Updated,
    Failed(FailureReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FailureReason {
    Cloud(CloudError),
    CannotDetermineAccountIdentity,
    AccountIdentityMismatch,
    InvalidClientIdentity,
    InvalidCertificate,
    CertificateNotCurrentlyValid,
}

impl fmt::Display for FailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cloud(error) => error.fmt(formatter),
            Self::CannotDetermineAccountIdentity => formatter
                .write_str("Cannot determine account identity required for gateway certificate"),
            Self::AccountIdentityMismatch => {
                formatter.write_str("Cloud account identity does not match stored credentials")
            }
            Self::InvalidClientIdentity => {
                formatter.write_str("Gateway certificate client identity is invalid")
            }
            Self::InvalidCertificate => {
                formatter.write_str("Gateway certificate response is invalid")
            }
            Self::CertificateNotCurrentlyValid => {
                formatter.write_str("Gateway certificate is not currently valid")
            }
        }
    }
}

impl StdError for FailureReason {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthReport {
    pub authentication: AuthenticationState,
    pub certificate: Option<CertificateValidity>,
    pub certificate_update: CertificateUpdate,
    completed_at: i64,
}

impl AuthReport {
    pub fn is_success(&self) -> bool {
        matches!(self.authentication, AuthenticationState::Authenticated)
            && self
                .certificate
                .is_some_and(|validity| validity.currently_valid(self.completed_at))
            && !matches!(self.certificate_update, CertificateUpdate::Failed(_))
    }

    pub fn completed_at(&self) -> i64 {
        self.completed_at
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        authentication: AuthenticationState,
        certificate: Option<CertificateValidity>,
        certificate_update: CertificateUpdate,
        completed_at: i64,
    ) -> Self {
        Self {
            authentication,
            certificate,
            certificate_update,
            completed_at,
        }
    }

    fn failed_auth(
        reason: FailureReason,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        Self {
            authentication: AuthenticationState::Unavailable(reason),
            certificate,
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at,
        }
    }

    fn failed_cloud_auth(
        error: CloudError,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        let authentication = if error.is_unauthorized() {
            AuthenticationState::SignInRequired(FailureReason::Cloud(error))
        } else {
            AuthenticationState::Unavailable(FailureReason::Cloud(error))
        };
        Self {
            authentication,
            certificate,
            certificate_update: CertificateUpdate::NotNeeded,
            completed_at,
        }
    }

    fn failed_certificate(
        reason: FailureReason,
        certificate: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Self {
        let authentication = match &reason {
            FailureReason::Cloud(error) if error.is_unauthorized() => {
                AuthenticationState::SignInRequired(reason.clone())
            }
            _ => AuthenticationState::Authenticated,
        };
        Self {
            authentication,
            certificate,
            certificate_update: CertificateUpdate::Failed(reason),
            completed_at,
        }
    }
}

#[derive(Debug)]
pub enum AuthError {
    Storage(StorageError),
    Cloud(CloudError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Cloud(error) => error.fmt(formatter),
        }
    }
}

impl StdError for AuthError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Cloud(error) => Some(error),
        }
    }
}

impl From<StorageError> for AuthError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

enum RequestFailure {
    SignInRequired(FailureReason),
    Unavailable(FailureReason),
    Storage(StorageError),
}

#[cfg(test)]
mod tests;
