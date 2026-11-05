use super::{AccountErrorKind, AccountStage, AuthError, AuthService, FailureReason};
use crate::{
    storage::{RecoveryStop, XiaomiRecord},
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
    ) -> Result<MijiaSession, AuthError> {
        if let Some(session) = &record.mijia {
            match self.validate_mijia(&record.uid, session).await {
                Ok(()) => return Ok(session.clone()),
                Err(AuthError::Cloud(error)) if error.is_unauthorized() => (),
                Err(error) => return Err(error),
            }
        }
        let recovery = record
            .recovery
            .as_ref()
            .ok_or(AuthError::Failure(FailureReason::MissingMijiaSession))?;
        if let Some(stop) = recovery.stop {
            return Err(AuthError::Failure(FailureReason::RecoveryStopped(stop)));
        }
        let candidate = match self.account.restore(&recovery.credentials).await {
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
}
