mod report;
mod tokens;

pub use report::{AuthError, AuthReport, AuthenticationState, CertificateUpdate, FailureReason};

use crate::{
    storage::{AuthRevision, StorageError, TokenSet, XiaomiRecord, XiaomiStore},
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
        let (previous, revision) = self.load_validated()?;
        let authorization = AuthorizationAttempt::new(
            previous
                .as_ref()
                .map(|candidate| candidate.record.oauth_client_uuid.as_str()),
        )
        .map_err(AuthError::Cloud)?;
        Ok(LoginAttempt {
            previous,
            revision,
            authorization,
        })
    }

    pub fn local_status(&self) -> Result<AuthReport, AuthError> {
        let (status, _) = self.load_validated()?;
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
        let tokens = match tokens::from_response(response, (self.now)()) {
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
            if self
                .store
                .replace_if_revision(attempt.revision, &record)
                .map_err(AuthError::Storage)?
                .is_none()
            {
                return self.local_status();
            }
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
        if self
            .store
            .replace_if_revision(attempt.revision, &record)
            .map_err(AuthError::Storage)?
            .is_none()
        {
            return self.local_status();
        }
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }

    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        let (stored, _) = self.load_validated()?;
        let Some(stored) = stored else {
            return Ok(AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        };
        let mut revision = stored.revision;
        let record = stored.record;
        let old_validity = stored.validity;
        let mut tokens = record.tokens.clone();
        let mut refreshed = false;

        if (self.now)() >= tokens.refresh_at
            && let Err(failure) = self
                .refresh_once(&record, &mut tokens, &mut refreshed, &mut revision)
                .await
        {
            return self.request_failure_report(failure, old_validity);
        }

        let home = match self.cloud.get_home(&tokens.access_token).await {
            Ok(home) => home,
            Err(error) if error.is_unauthorized() && !refreshed => {
                if let Err(failure) = self
                    .refresh_once(&record, &mut tokens, &mut refreshed, &mut revision)
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
                    .refresh_once(&record, &mut tokens, &mut refreshed, &mut revision)
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
            if self.store.revision().map_err(AuthError::Storage)? != revision {
                return self.local_status();
            }
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
                .refresh_once(&record, &mut tokens, &mut refreshed, &mut revision)
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
        if self
            .store
            .update_certificate_if_revision(revision, &certificate_pem)
            .map_err(AuthError::Storage)?
            .is_none()
        {
            return self.local_status();
        }
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(new_validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }

    async fn refresh_once(
        &self,
        record: &XiaomiRecord,
        tokens: &mut TokenSet,
        refreshed: &mut bool,
        revision: &mut AuthRevision,
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
        let replacement = tokens::from_response(response, (self.now)())
            .map_err(|error| RequestFailure::Unavailable(FailureReason::Cloud(error)))?;
        *revision = self
            .store
            .update_tokens_if_revision(*revision, &replacement)
            .map_err(RequestFailure::Storage)?
            .ok_or(RequestFailure::Superseded)?;
        *tokens = replacement;
        Ok(())
    }

    fn load_validated(&self) -> Result<(Option<StoredRecord>, AuthRevision), AuthError> {
        let snapshot = self.store.snapshot().map_err(AuthError::Storage)?;
        let Some(record) = snapshot.record else {
            return Ok((None, snapshot.revision));
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
        Ok((
            Some(StoredRecord {
                record,
                validity,
                revision: snapshot.revision,
            }),
            snapshot.revision,
        ))
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
            RequestFailure::Superseded => self.local_status(),
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
            RequestFailure::Superseded => self.local_status(),
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
    revision: AuthRevision,
}

pub struct LoginAttempt {
    previous: Option<StoredRecord>,
    authorization: AuthorizationAttempt,
    revision: AuthRevision,
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

enum RequestFailure {
    SignInRequired(FailureReason),
    Unavailable(FailureReason),
    Storage(StorageError),
    Superseded,
}

#[cfg(test)]
mod tests;
