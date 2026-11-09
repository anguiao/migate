use super::{
    AccountErrorKind, AccountStage, AuthError, AuthReport, AuthService, AuthenticationState,
    CertificateUpdate, FailureReason,
    service::{StoredRecord, verify_account_uid},
};
use crate::{
    storage::{TokenSet, XiaomiRecord},
    xiaomi::{certificate::CertificateValidity, cloud::CloudError},
};

impl AuthService {
    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        let stored = self.load_validated()?;
        let mut report = self.report_for(stored.as_ref());
        let Some(stored) = stored else {
            return Ok(report);
        };
        report.mijia = match self.maintain_mijia(&stored.record).await {
            Ok(_) => AuthenticationState::Authenticated,
            Err(error) => state_for_error(error)?,
        };
        report.recovery_stop = self
            .store
            .current_snapshot()
            .record
            .and_then(|record| record.recovery.stop);
        self.check_ha_and_certificate(stored, &mut report).await?;
        report.set_completed_at((self.now)());
        Ok(report)
    }

    pub(super) async fn check_ha_and_certificate(
        &self,
        stored: StoredRecord,
        report: &mut AuthReport,
    ) -> Result<(), AuthError> {
        let record = stored.record;
        let Some(authorization) = record.ha_authorization() else {
            return Ok(());
        };
        let mut session = CheckSession {
            auth: self,
            redirect_uri: &authorization.redirect_uri,
            tokens: authorization.tokens.clone(),
            refreshed: false,
        };
        if (self.now)() >= session.tokens.refresh_at
            && let Err(error) = session.refresh().await
        {
            report.ha = state_for_error(error)?;
            return Ok(());
        }
        let home = match session
            .request(async |token| self.cloud.get_home(token).await)
            .await
        {
            Ok(home) => home,
            Err(error) => {
                report.ha = state_for_error(error.into_error())?;
                return Ok(());
            }
        };
        if let Err(reason) = verify_account_uid(&record.uid, home.uid.as_deref()) {
            report.ha = AuthenticationState::Unavailable(reason);
            return Ok(());
        }
        report.ha = AuthenticationState::Authenticated;
        self.maintain_certificate(&record, stored.validity, &mut session, report)
            .await?;
        Ok(())
    }

    pub(super) async fn maintain_certificate(
        &self,
        record: &XiaomiRecord,
        validity: Option<CertificateValidity>,
        session: &mut CheckSession<'_>,
        report: &mut AuthReport,
    ) -> Result<(), AuthError> {
        if validity.is_some_and(|validity| {
            validity.currently_valid((self.now)()) && !validity.renewal_due((self.now)())
        }) {
            report.certificate_update = CertificateUpdate::NotNeeded;
            return Ok(());
        }
        let mut record = record.clone();
        let identity = match self.prepare_client_identity(&mut record) {
            Ok(identity) => identity,
            Err(AuthError::Failure(reason)) => {
                report.certificate_update = CertificateUpdate::Failed(reason);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let response = session
            .request(async |token| self.cloud.get_certificate(token, &identity.csr_pem).await)
            .await;
        let certificate = match response {
            Ok(certificate) => certificate,
            Err(error) => {
                let refresh_failed = matches!(error, RequestFailure::Refresh(_));
                let state = state_for_error(error.into_error())?;
                let reason = match &state {
                    AuthenticationState::SignInRequired(reason)
                    | AuthenticationState::Unavailable(reason) => reason.clone(),
                    _ => unreachable!(),
                };
                if refresh_failed || matches!(state, AuthenticationState::SignInRequired(_)) {
                    report.ha = state;
                }
                report.certificate_update = CertificateUpdate::Failed(reason);
                return Ok(());
            }
        };
        match self.save_certificate_response(&record, &certificate, (self.now)())? {
            Ok(validity) => {
                report.certificate = Some(validity);
                report.certificate_update = CertificateUpdate::Updated;
            }
            Err(reason) => report.certificate_update = CertificateUpdate::Failed(reason),
        }
        Ok(())
    }
}

pub(super) fn state_for_error(error: AuthError) -> Result<AuthenticationState, AuthError> {
    let (reason, needs_login) = match error {
        AuthError::Cloud(error) => {
            let rejected = error.is_unauthorized();
            (FailureReason::Cloud(error), rejected)
        }
        AuthError::Account(error) => {
            let rejected = error.kind() == AccountErrorKind::VerificationRequired
                || (error.stage() == AccountStage::ServiceLogin
                    && error.kind() == AccountErrorKind::Rejected);
            (FailureReason::Account(error), rejected)
        }
        AuthError::Failure(reason) => {
            let rejected = matches!(reason, FailureReason::RecoveryStopped(_));
            (reason, rejected)
        }
        error => return Err(error),
    };
    Ok(if needs_login {
        AuthenticationState::SignInRequired(reason)
    } else {
        AuthenticationState::Unavailable(reason)
    })
}

/// A check or login shares one refresh allowance across all HA requests.
pub(super) struct CheckSession<'a> {
    pub(super) auth: &'a AuthService,
    pub(super) redirect_uri: &'a str,
    pub(super) tokens: TokenSet,
    pub(super) refreshed: bool,
}
impl CheckSession<'_> {
    async fn request<T>(
        &mut self,
        mut request: impl AsyncFnMut(&str) -> Result<T, CloudError>,
    ) -> Result<T, RequestFailure> {
        let mut result = request(&self.tokens.access_token).await;
        if result.as_ref().is_err_and(|error| error.is_unauthorized()) && !self.refreshed {
            self.refresh().await.map_err(RequestFailure::Refresh)?;
            result = request(&self.tokens.access_token).await;
        }
        if result.as_ref().is_err_and(|error| error.is_unauthorized()) {
            self.auth
                .store
                .stop_ha_refresh()
                .map_err(|error| RequestFailure::Other(error.into()))?;
        }
        result.map_err(|error| RequestFailure::Other(AuthError::Cloud(error)))
    }
    async fn refresh(&mut self) -> Result<(), AuthError> {
        self.tokens = self
            .auth
            .refresh_tokens_once(self.redirect_uri, &self.tokens, self.auth.now)
            .await?;
        self.refreshed = true;
        Ok(())
    }
}

enum RequestFailure {
    Refresh(AuthError),
    Other(AuthError),
}
impl RequestFailure {
    fn into_error(self) -> AuthError {
        match self {
            Self::Refresh(error) | Self::Other(error) => error,
        }
    }
}
