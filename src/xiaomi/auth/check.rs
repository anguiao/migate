use super::{
    AuthError, AuthReport, AuthService, AuthenticationState, CertificateUpdate, FailureReason,
    service::StoredRecord,
};
use crate::{
    storage::{StorageError, TokenSet},
    xiaomi::{certificate::CertificateValidity, cloud::CloudError},
};

impl AuthService {
    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        let Some(stored) = self.load_validated()? else {
            return Ok(AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        };
        let certificate_update = stored.certificate_update.clone();
        let mut report = self.check_stored(stored).await?;
        if report.certificate.is_none() {
            report.certificate_update = certificate_update;
        }
        Ok(report)
    }

    async fn check_stored(&self, stored: StoredRecord) -> Result<AuthReport, AuthError> {
        let record = stored.record;
        let old_validity = stored.validity;
        let certificate_update = stored.certificate_update;
        if record.ha.as_ref().is_some_and(|ha| ha.stopped) {
            return Ok(AuthReport {
                authentication: AuthenticationState::SignInRequired(
                    FailureReason::MissingHaAuthorization,
                ),
                certificate: old_validity,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        }
        let Some(ha) = record.ha.as_ref().filter(|ha| !ha.stopped) else {
            return Ok(AuthReport::failed_auth(
                FailureReason::MissingHaAuthorization,
                old_validity,
                (self.now)(),
            ));
        };
        let Some(authorization) = ha.authorization.as_ref() else {
            return Ok(AuthReport::failed_auth(
                FailureReason::MissingHaAuthorization,
                old_validity,
                (self.now)(),
            ));
        };
        let mut session = CheckSession {
            auth: self,
            redirect_uri: &authorization.redirect_uri,
            tokens: authorization.tokens.clone(),
            refreshed: false,
        };

        if (self.now)() >= session.tokens.refresh_at
            && let Err(failure) = session.refresh().await
        {
            return failure.auth_report(old_validity, (self.now)());
        }

        let home = match session
            .request(async |token| self.cloud.get_home(token).await)
            .await
        {
            Ok(home) => home,
            Err(failure) => return failure.auth_report(old_validity, (self.now)()),
        };

        if let Err(failure) = session
            .request(async |token| self.cloud.get_devices(token, &home.dids).await)
            .await
        {
            return failure.auth_report(old_validity, (self.now)());
        }

        let Some(uid) = home.uid else {
            return Ok(AuthReport::failed_auth(
                FailureReason::CannotDetermineAccountIdentity,
                old_validity,
                (self.now)(),
            ));
        };
        if uid != record.uid {
            return Ok(AuthReport::failed_auth(
                FailureReason::AccountIdentityMismatch,
                old_validity,
                (self.now)(),
            ));
        }

        let renewal_time = (self.now)();
        let Some(old_validity) = old_validity else {
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: None,
                certificate_update,
                completed_at: renewal_time,
            });
        };
        if !old_validity.renewal_due(renewal_time) {
            return Ok(AuthReport {
                authentication: AuthenticationState::Authenticated,
                certificate: Some(old_validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            });
        }

        let identity = self.client_identity(&record)?;
        let certificate_pem = match session
            .request(async |token| self.cloud.get_certificate(token, &identity.csr_pem).await)
            .await
        {
            Ok(certificate) => certificate,
            Err(failure) => return failure.certificate_report(old_validity, (self.now)()),
        };
        let completed_at = (self.now)();
        let new_validity =
            match self.save_certificate_response(&record, &certificate_pem, completed_at)? {
                Ok(validity) => validity,
                Err(reason) => {
                    return Ok(AuthReport::failed_certificate(
                        reason,
                        Some(old_validity),
                        completed_at,
                    ));
                }
            };
        Ok(AuthReport {
            authentication: AuthenticationState::Authenticated,
            certificate: Some(new_validity),
            certificate_update: CertificateUpdate::Updated,
            completed_at,
        })
    }
}

/// One check shares a single refresh allowance across every protected request.
struct CheckSession<'a> {
    auth: &'a AuthService,
    redirect_uri: &'a str,
    tokens: TokenSet,
    refreshed: bool,
}

impl CheckSession<'_> {
    async fn request<T>(
        &mut self,
        mut request: impl AsyncFnMut(&str) -> Result<T, CloudError>,
    ) -> Result<T, RequestFailure> {
        match request(&self.tokens.access_token).await {
            Err(error) if error.is_unauthorized() && !self.refreshed => {
                self.refresh().await?;
                request(&self.tokens.access_token)
                    .await
                    .map_err(RequestFailure::Protected)
            }
            result => result.map_err(RequestFailure::Protected),
        }
    }

    async fn refresh(&mut self) -> Result<(), RequestFailure> {
        self.refreshed = true;
        let replacement = self
            .auth
            .refresh_tokens_once(self.redirect_uri, &self.tokens, self.auth.now)
            .await
            .map_err(|error| match error {
                AuthError::Cloud(error) => RequestFailure::Refresh(error),
                AuthError::Storage(error) => RequestFailure::Storage(error),
                AuthError::Missing(message) => RequestFailure::Missing(message),
            })?;
        self.tokens = replacement;
        Ok(())
    }
}

enum RequestFailure {
    Protected(CloudError),
    Refresh(CloudError),
    Storage(StorageError),
    Missing(&'static str),
}

impl RequestFailure {
    fn auth_report(
        self,
        validity: Option<CertificateValidity>,
        completed_at: i64,
    ) -> Result<AuthReport, AuthError> {
        match self {
            Self::Protected(error) | Self::Refresh(error) => {
                Ok(AuthReport::failed_cloud_auth(error, validity, completed_at))
            }
            Self::Storage(error) => Err(AuthError::Storage(error)),
            Self::Missing(message) => Err(AuthError::Missing(message)),
        }
    }

    fn certificate_report(
        self,
        validity: CertificateValidity,
        completed_at: i64,
    ) -> Result<AuthReport, AuthError> {
        match self {
            Self::Protected(error) => Ok(AuthReport::failed_certificate(
                FailureReason::Cloud(error),
                Some(validity),
                completed_at,
            )),
            Self::Refresh(error) => {
                let mut report =
                    AuthReport::failed_cloud_auth(error.clone(), Some(validity), completed_at);
                report.certificate_update = CertificateUpdate::Failed(FailureReason::Cloud(error));
                Ok(report)
            }
            Self::Storage(error) => Err(AuthError::Storage(error)),
            Self::Missing(message) => Err(AuthError::Missing(message)),
        }
    }
}
