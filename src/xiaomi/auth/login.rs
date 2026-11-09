use super::{
    AccountClientIdentity, AccountError, AccountErrorKind, AccountStage, AuthError, AuthReport,
    AuthService, AuthenticationState, FailureReason, LoginFailure, QrAttempt,
    check::CheckSession,
    service::{StoredRecord, verify_account_uid},
    tokens,
};
use crate::{storage::HaAuthorization, xiaomi::cloud::AuthorizationAttempt};
use std::{fmt, time::Duration};

pub struct LoginAttempt {
    previous: Option<StoredRecord>,
    authorization: AuthorizationAttempt,
    qr: QrAttempt,
}
impl LoginAttempt {
    pub fn login_url(&self) -> &str {
        self.qr.login_url()
    }
    pub fn remaining(&self) -> Duration {
        self.qr.remaining()
    }
}
impl fmt::Debug for LoginAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginAttempt")
            .field("has_previous", &self.previous.is_some())
            .field("qr", &self.qr)
            .finish()
    }
}

impl AuthService {
    pub async fn begin_login(&self) -> Result<LoginAttempt, AuthError> {
        let previous = self.load_validated()?;
        let authorization = AuthorizationAttempt::new(
            previous
                .as_ref()
                .map(|value| value.record.ha.client_uuid.as_str()),
        )?;
        let identity = previous
            .as_ref()
            .map(|value| value.record.recovery.credentials.identity().clone())
            .unwrap_or_else(AccountClientIdentity::generate);
        let qr = self.account.start_qr(identity).await?;
        Ok(LoginAttempt {
            previous,
            authorization,
            qr,
        })
    }

    pub async fn complete_login(&self, attempt: LoginAttempt) -> Result<AuthReport, AuthError> {
        let expected_uid = attempt
            .previous
            .as_ref()
            .map(|stored| stored.record.uid.as_str());
        let account = match attempt.qr.poll(expected_uid).await {
            Ok(account) => account,
            Err(error) => return self.login_failure("QR login and STS", account_reason(error)),
        };
        let uid = account.credentials().user_id().to_owned();
        if let Err(error) = self.validate_mijia(&uid, account.session()).await {
            return self.login_error("Mijia session verification", error);
        }
        let grant = match account.authorize(&attempt.authorization).await {
            Ok(grant) => grant,
            Err(error) => {
                return self.login_failure("HA OAuth authorization", account_reason(error));
            }
        };
        let response = match self
            .cloud
            .exchange_token(&attempt.authorization, grant.code())
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return self.login_failure("HA OAuth token exchange", FailureReason::Cloud(error));
            }
        };
        let tokens = match tokens::from_response(response, (self.now)()) {
            Ok(tokens) => tokens,
            Err(error) => {
                return self.login_failure("HA OAuth token exchange", FailureReason::Cloud(error));
            }
        };
        let home = match self.cloud.get_home(&tokens.access_token).await {
            Ok(home) => home,
            Err(error) => {
                return self
                    .login_failure("HA OAuth account verification", FailureReason::Cloud(error));
            }
        };
        if let Err(reason) = verify_account_uid(&uid, home.uid.as_deref()) {
            return self.login_failure("HA OAuth account verification", reason);
        }
        let (credentials, mijia, _) = grant.into_parts();
        let authorization = HaAuthorization {
            redirect_uri: attempt.authorization.redirect_uri().to_owned(),
            tokens,
        };
        self.store
            .commit_login(
                &credentials,
                &mijia,
                attempt.authorization.oauth_client_uuid(),
                &authorization,
            )
            .map_err(|error| login_step("credential commit", error.into()))?;

        let stored = self
            .reload_credentials()
            .map_err(|error| login_step("certificate preparation", error))?;
        let mut report = self.report_for(Some(&stored));
        report.mijia = AuthenticationState::Authenticated;
        report.ha = AuthenticationState::Authenticated;
        let mut session = CheckSession {
            auth: self,
            redirect_uri: &authorization.redirect_uri,
            tokens: authorization.tokens,
            refreshed: false,
        };
        self.maintain_certificate(&stored.record, stored.validity, &mut session, &mut report)
            .await
            .map_err(|error| login_step("certificate preparation", error))?;
        report.set_completed_at((self.now)());
        Ok(report)
    }

    fn login_error(&self, stage: &'static str, error: AuthError) -> Result<AuthReport, AuthError> {
        match error {
            AuthError::Cloud(error) => self.login_failure(stage, FailureReason::Cloud(error)),
            AuthError::Account(error) => self.login_failure(stage, account_reason(error)),
            AuthError::Failure(reason) => self.login_failure(stage, reason),
            error => Err(error),
        }
    }
    fn login_failure(
        &self,
        stage: &'static str,
        reason: FailureReason,
    ) -> Result<AuthReport, AuthError> {
        let mut report = self.local_status()?;
        report.login_failure = Some(LoginFailure { stage, reason });
        Ok(report)
    }
}

fn account_reason(error: AccountError) -> FailureReason {
    if error.kind() == AccountErrorKind::IdentityMismatch {
        if error.stage() == AccountStage::QrPolling {
            FailureReason::DifferentScannedAccount
        } else {
            FailureReason::AccountIdentityMismatch
        }
    } else {
        FailureReason::Account(error)
    }
}

fn login_step(stage: &'static str, source: AuthError) -> AuthError {
    AuthError::LoginStep {
        stage,
        source: Box::new(source),
    }
}
