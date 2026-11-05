use super::{AccountErrorKind, AccountStage, AuthError, AuthService, FailureReason};
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
        match homes.uid.as_deref() {
            Some(actual) if actual == uid => Ok(()),
            Some(_) => Err(AuthError::Failure(FailureReason::AccountIdentityMismatch)),
            None => Err(AuthError::Failure(
                FailureReason::CannotDetermineAccountIdentity,
            )),
        }
    }

    /// Validate the saved session and make at most one independent recovery attempt.
    /// The caller can classify Account and Cloud errors for its own retry policy.
    pub(crate) async fn maintain_mijia(
        &self,
        record: &XiaomiRecord,
        generation: &str,
    ) -> Result<MijiaSession, AuthError> {
        self.ensure_mijia_current(&record.uid, generation)?;
        if let Some(session) = &record.mijia {
            let result = self.validate_mijia(&record.uid, session).await;
            self.ensure_mijia_current(&record.uid, generation)?;
            match result {
                Ok(()) => return Ok(session.clone()),
                Err(AuthError::Cloud(error)) if error.is_unauthorized() => (),
                Err(error) => return Err(error),
            }
        }
        self.recover_mijia(record, generation).await
    }

    /// Perform one restoration after the caller has identified a rejected or
    /// missing session. The captured generation owns all resulting writes.
    pub(crate) async fn recover_mijia(
        &self,
        record: &XiaomiRecord,
        generation: &str,
    ) -> Result<MijiaSession, AuthError> {
        let recovery = self.current_recovery(&record.uid, generation)?;
        let result = self.account.restore(&recovery.credentials).await;
        self.current_recovery(&record.uid, generation)?;
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
                if let Some(stop) = stop
                    && !self
                        .store
                        .stop_recovery_if_current(&record.uid, generation, stop)?
                {
                    return Err(AuthError::Missing(
                        "Mijia credentials changed during recovery",
                    ));
                }
                return Err(AuthError::Account(error));
            }
        };
        if candidate.credentials().user_id() != record.uid {
            return Err(AuthError::Failure(FailureReason::AccountIdentityMismatch));
        }
        let validation = self.validate_mijia(&record.uid, candidate.session()).await;
        self.current_recovery(&record.uid, generation)?;
        validation?;
        let (credentials, session) = candidate.into_parts();
        if !self
            .store
            .update_recovery_if_current(&credentials, &session, generation)?
        {
            return Err(AuthError::Missing(
                "Mijia credentials changed during recovery",
            ));
        }
        Ok(session)
    }

    fn current_recovery(&self, uid: &str, generation: &str) -> Result<AccountRecovery, AuthError> {
        self.ensure_mijia_current(uid, generation)?;
        let recovery = self
            .store
            .load()?
            .and_then(|record| record.recovery)
            .ok_or(AuthError::Failure(FailureReason::MissingMijiaSession))?;
        if let Some(stop) = recovery.stop {
            return Err(AuthError::Failure(FailureReason::RecoveryStopped(stop)));
        }
        Ok(recovery)
    }

    fn ensure_mijia_current(&self, uid: &str, generation: &str) -> Result<(), AuthError> {
        if self.store.mijia_generation(uid)?.as_deref() != Some(generation) {
            return Err(AuthError::Missing(
                "Mijia credentials changed during recovery",
            ));
        }
        Ok(())
    }
}
