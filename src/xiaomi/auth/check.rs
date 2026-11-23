use super::{
    AccountErrorKind, AccountStage, AuthError, AuthReport, AuthService, AuthenticationState,
    CertificateUpdate, FailureReason,
    refresh::HA_CHANGED,
    service::{StoredRecord, verify_account_uid},
};
use crate::{
    storage::HaAuthorization,
    xiaomi::{certificate::CertificateValidity, cloud::CloudError},
};

impl AuthService {
    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        let Some(initial) = self.load_validated()? else {
            return Ok(self.report_for(None));
        };
        let refreshes = self.store.ha_refreshes();
        // Retain the attempt even if it finishes during the independent Mijia check.
        let pending = refreshes.in_flight(initial.ha_generation);
        let mijia = match self
            .maintain_mijia(&initial.record, initial.mijia_generation)
            .await
        {
            Ok(_) => AuthenticationState::Authenticated,
            Err(error) => state_for_error(error)?,
        };
        let pending = pending.or_else(|| {
            self.store
                .current_snapshot()
                .ha_generation()
                .and_then(|generation| refreshes.in_flight(generation))
        });
        let joined = match pending {
            Some(refresh) => Some(refresh.await),
            None => None,
        };
        let stored = self.reload_credentials()?;
        let mut report = self.report_for(Some(&stored));
        report.mijia = mijia;
        if let Some(Err(error)) = joined {
            report.ha = state_for_error(error)?;
        } else {
            let refreshed = joined.is_some() || stored.ha_generation != initial.ha_generation;
            self.check_ha_and_certificate(stored, refreshed, &mut report)
                .await?;
        }
        report.set_completed_at((self.now)());
        Ok(report)
    }

    async fn check_ha_and_certificate(
        &self,
        stored: StoredRecord,
        refreshed: bool,
        report: &mut AuthReport,
    ) -> Result<(), AuthError> {
        let Some(mut session) = CheckSession::new(self, &stored, refreshed) else {
            return Ok(());
        };
        if !session.refreshed
            && (self.now)() >= session.authorization.tokens.refresh_at
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
        if let Err(reason) = verify_account_uid(session.uid, home.uid.as_deref()) {
            report.ha = AuthenticationState::Unavailable(reason);
            return Ok(());
        }
        report.ha = AuthenticationState::Authenticated;
        self.maintain_certificate(stored.validity, &mut session, report)
            .await?;
        Ok(())
    }

    pub(super) async fn maintain_certificate(
        &self,
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
        let (record, identity) = match self.prepare_client_identity(session.uid, session.generation)
        {
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
            let rejected = matches!(
                reason,
                FailureReason::RecoveryStopped(_) | FailureReason::HaAuthorizationStopped
            );
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
    auth: &'a AuthService,
    uid: &'a str,
    authorization: HaAuthorization,
    login_generation: u64,
    generation: u64,
    refreshed: bool,
}
impl<'a> CheckSession<'a> {
    pub(super) fn new(
        auth: &'a AuthService,
        stored: &'a StoredRecord,
        refreshed: bool,
    ) -> Option<Self> {
        Some(Self {
            auth,
            uid: &stored.record.uid,
            authorization: stored.record.ha_authorization()?.clone(),
            login_generation: stored.login_generation,
            generation: stored.ha_generation,
            refreshed,
        })
    }

    fn adopt_rotation(&mut self) -> Result<bool, AuthError> {
        if self.auth.store.current_ha(self.uid, self.generation) {
            return Ok(false);
        }
        let snapshot = self.auth.store.current_snapshot();
        let record = snapshot
            .record
            .filter(|record| {
                record.uid == self.uid && snapshot.login_generation == self.login_generation
            })
            .ok_or(HA_CHANGED)?;
        if record.ha.stopped {
            return Err(AuthError::Failure(FailureReason::HaAuthorizationStopped));
        }
        self.authorization = record.ha.authorization;
        self.generation = snapshot.ha_generation;
        self.refreshed = true;
        Ok(true)
    }

    async fn request<T>(
        &mut self,
        mut request: impl AsyncFnMut(&str) -> Result<T, CloudError>,
    ) -> Result<T, RequestFailure> {
        self.adopt_rotation().map_err(RequestFailure::Other)?;
        loop {
            let result = request(&self.authorization.tokens.access_token).await;
            if self.adopt_rotation().map_err(RequestFailure::Other)? {
                continue;
            }
            if result.as_ref().is_err_and(|error| error.is_unauthorized()) {
                if !self.refreshed {
                    self.refresh().await.map_err(RequestFailure::Refresh)?;
                    continue;
                }
                self.auth
                    .store
                    .stop_ha_refresh()
                    .map_err(|error| RequestFailure::Other(error.into()))?;
            }
            return result.map_err(|error| RequestFailure::Other(AuthError::Cloud(error)));
        }
    }

    async fn refresh(&mut self) -> Result<(), AuthError> {
        let refreshed = self
            .auth
            .refresh_tokens_once(
                self.uid,
                self.generation,
                &self.authorization,
                self.auth.now,
            )
            .await?;
        self.authorization.tokens = refreshed.tokens;
        self.generation = refreshed.generation;
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
