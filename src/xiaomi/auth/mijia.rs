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
    pub(super) async fn maintain_mijia(
        &self,
        record: &XiaomiRecord,
        generation: u64,
    ) -> Result<MijiaSession, AuthError> {
        match self.validate_mijia(&record.uid, &record.mijia).await {
            Ok(()) => Ok(record.mijia.clone()),
            Err(AuthError::Cloud(error)) if error.is_unauthorized() => {
                self.recover_mijia(&record.uid, generation).await
            }
            Err(error) => Err(error),
        }
    }

    /// Perform one restoration after the caller has identified a rejected session.
    pub(crate) async fn recover_mijia(
        &self,
        uid: &str,
        generation: u64,
    ) -> Result<MijiaSession, AuthError> {
        self.require_mijia(uid, generation)?;
        let recovery = self.current_recovery()?;
        let result = self.account.restore(&recovery.credentials).await;
        self.require_mijia(uid, generation)?;
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
        if candidate.credentials().user_id() != uid {
            return Err(AuthError::Failure(FailureReason::AccountIdentityMismatch));
        }
        let validation = self.validate_mijia(uid, candidate.session()).await;
        self.require_mijia(uid, generation)?;
        validation?;
        let (credentials, session) = candidate.into_parts();
        self.store.update_recovery(&credentials, &session)?;
        Ok(session)
    }

    fn require_mijia(&self, uid: &str, generation: u64) -> Result<(), AuthError> {
        if self.store.current_mijia(uid, generation) {
            Ok(())
        } else {
            Err(AuthError::Missing(
                "Mijia credentials changed during recovery",
            ))
        }
    }

    fn current_recovery(&self) -> Result<AccountRecovery, AuthError> {
        let recovery = self
            .store
            .current_snapshot()
            .record
            .ok_or(AuthError::Missing("Xiaomi account is unavailable"))?
            .recovery;
        if let Some(stop) = recovery.stop {
            return Err(AuthError::Failure(FailureReason::RecoveryStopped(stop)));
        }
        Ok(recovery)
    }
}
