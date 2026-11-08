use super::{
    AccountErrorKind, AccountStage, AuthError, AuthService, FailureReason,
    service::verify_account_uid,
};
use crate::{
    storage::{AccountRecovery, RecoveryStop, XiaomiRecord},
    xiaomi::mijia::MijiaSession,
};

impl AuthService {
    pub(super) async fn validate_mijia(
        &self,
        uid: &str,
        session: &MijiaSession,
    ) -> Result<(), AuthError> {
        if session.user_id() != uid {
            return Err(AuthError::Failure(FailureReason::AccountIdentityMismatch));
        }
        let homes = self.mijia.get_owned_homes(session).await?;
        verify_account_uid(uid, homes.uid.as_deref()).map_err(AuthError::Failure)
    }

    /// Validate the saved session and make at most one independent recovery attempt.
    /// The caller can classify Account and Cloud errors for its own retry policy.
    pub(crate) async fn maintain_mijia(
        &self,
        record: &XiaomiRecord,
    ) -> Result<MijiaSession, AuthError> {
        if let Some(session) = &record.mijia {
            let result = self.validate_mijia(&record.uid, session).await;
            match result {
                Ok(()) => return Ok(session.clone()),
                Err(AuthError::Cloud(error)) if error.is_unauthorized() => (),
                Err(error) => return Err(error),
            }
        }
        self.recover_mijia(record).await
    }

    /// Perform one restoration after the caller has identified a rejected or
    /// missing session.
    pub(crate) async fn recover_mijia(
        &self,
        record: &XiaomiRecord,
    ) -> Result<MijiaSession, AuthError> {
        let recovery = self.current_recovery()?;
        let result = self.account.restore(&recovery.credentials).await;
        let candidate = match result {
            Ok(candidate) => candidate,
            Err(error) => {
                let stop = match (error.stage(), error.kind()) {
                    (AccountStage::ServiceLogin, AccountErrorKind::Rejected) => {
                        Some(RecoveryStop::Rejected)
                    }
                    (_, AccountErrorKind::VerificationRequired) => {
                        Some(RecoveryStop::VerificationRequired)
                    }
                    _ => None,
                };
                if let Some(stop) = stop {
                    self.store.stop_recovery(stop)?;
                }
                return Err(AuthError::Account(error));
            }
        };
        if candidate.credentials().user_id() != record.uid {
            return Err(AuthError::Failure(FailureReason::AccountIdentityMismatch));
        }
        self.validate_mijia(&record.uid, candidate.session())
            .await?;
        let (credentials, session) = candidate.into_parts();
        self.store.update_recovery(&credentials, &session)?;
        Ok(session)
    }

    fn current_recovery(&self) -> Result<AccountRecovery, AuthError> {
        let recovery = self
            .store
            .current_snapshot()
            .record
            .and_then(|record| record.recovery)
            .ok_or(AuthError::Failure(FailureReason::MissingMijiaSession))?;
        if let Some(stop) = recovery.stop {
            return Err(AuthError::Failure(FailureReason::RecoveryStopped(stop)));
        }
        Ok(recovery)
    }
}
