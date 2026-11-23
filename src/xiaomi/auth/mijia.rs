use super::{
    AccountErrorKind, AccountStage, AuthError, AuthService, FailureReason,
    service::{StoredRecord, verify_account_uid},
};
use crate::{
    storage::{AccountRecovery, RecoveryStop},
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
    pub(super) async fn maintain_mijia(&self, stored: &StoredRecord) -> Result<(), AuthError> {
        let record = &stored.record;
        let result = match self.validate_mijia(&record.uid, &record.mijia).await {
            Err(AuthError::Cloud(error)) if error.is_unauthorized() => self
                .recover_mijia(&record.uid, stored.mijia_generation)
                .await
                .map(drop),
            result => result,
        };
        let current = self.store.current_snapshot();
        if current.login_generation != stored.login_generation {
            return Err(AuthError::Missing(
                "Xiaomi login changed during the Mijia check",
            ));
        }
        if current.mijia_generation == stored.mijia_generation {
            return result;
        }
        // A recovery validates the new session before advancing its generation.
        match current.record.expect("same saved login").recovery.stop {
            Some(stop) => Err(AuthError::Failure(FailureReason::RecoveryStopped(stop))),
            None => Ok(()),
        }
    }

    /// Perform one restoration after the caller has identified a rejected session.
    pub(crate) async fn recover_mijia(
        &self,
        uid: &str,
        generation: u64,
    ) -> Result<MijiaSession, AuthError> {
        let recovery = self.current_recovery(uid, generation)?;
        let result = self.account.restore(&recovery.credentials).await;
        self.current_recovery(uid, generation)?;
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
        self.current_recovery(uid, generation)?;
        validation?;
        let (credentials, session) = candidate.into_parts();
        self.store.update_recovery(&credentials, &session)?;
        Ok(session)
    }

    fn current_recovery(&self, uid: &str, generation: u64) -> Result<AccountRecovery, AuthError> {
        if !self.store.current_mijia(uid, generation) {
            return Err(AuthError::Missing(
                "Mijia credentials changed during recovery",
            ));
        }
        let recovery = self
            .store
            .current_snapshot()
            .record
            .expect("current Mijia session")
            .recovery;
        if let Some(stop) = recovery.stop {
            return Err(AuthError::Failure(FailureReason::RecoveryStopped(stop)));
        }
        Ok(recovery)
    }
}
