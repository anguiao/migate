use super::{
    AuthError, AuthReport, AuthService, AuthenticationState, CertificateUpdate, FailureReason,
    service::StoredRecord, tokens,
};
use crate::{
    storage::XiaomiRecord,
    xiaomi::{
        certificate::{ClientIdentity, validate_certificate},
        cloud::{AuthorizationAttempt, REGION},
    },
};

use std::fmt;

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

impl AuthService {
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
}
