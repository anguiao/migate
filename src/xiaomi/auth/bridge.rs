use super::{AuthError, AuthService, CertificateUpdate, FailureReason};
use crate::{storage::XiaomiRecord, xiaomi::certificate::CertificateValidity};

#[derive(Debug)]
pub(crate) struct BridgeCertificate {
    pub(crate) credentials: XiaomiRecord,
    pub(crate) validity: Option<CertificateValidity>,
    pub(crate) update: CertificateUpdate,
}

impl AuthService {
    pub(crate) async fn validate_bridge_ha(&self, record: &XiaomiRecord) -> Result<(), AuthError> {
        let authorization = record
            .ha_authorization()
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        let home = self
            .cloud
            .get_home(&authorization.tokens.access_token)
            .await?;
        match home.uid.as_deref() {
            Some(uid) if uid == record.uid => Ok(()),
            Some(_) => Err(AuthError::Failure(FailureReason::AccountIdentityMismatch)),
            None => Err(AuthError::Failure(
                FailureReason::CannotDetermineAccountIdentity,
            )),
        }
    }

    pub(crate) async fn refresh_bridge_tokens(
        &self,
        record: &XiaomiRecord,
        now: impl Fn() -> i64,
    ) -> Result<XiaomiRecord, AuthError> {
        let authorization = record
            .ha
            .as_ref()
            .filter(|ha| !ha.stopped)
            .and_then(|ha| ha.authorization.as_ref())
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        self.refresh_tokens_once(&authorization.redirect_uri, &authorization.tokens, now)
            .await?;
        Ok(self.reload_credentials()?.record)
    }

    pub(crate) async fn renew_bridge_certificate(
        &self,
        record: &XiaomiRecord,
        now: impl Fn() -> i64,
    ) -> Result<super::BridgeCertificate, AuthError> {
        let identity = self.client_identity(record)?;
        let authorization = record
            .ha
            .as_ref()
            .filter(|ha| !ha.stopped)
            .and_then(|ha| ha.authorization.as_ref())
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        let certificate = self
            .cloud
            .get_certificate(&authorization.tokens.access_token, &identity.csr_pem)
            .await
            .map_err(AuthError::Cloud)?;
        let update = match self.save_certificate_response(record, &certificate, now())? {
            Ok(_) => CertificateUpdate::Updated,
            Err(reason) => CertificateUpdate::Failed(reason),
        };
        let stored = self.reload_credentials()?;
        Ok(BridgeCertificate {
            credentials: stored.record,
            validity: stored.validity,
            update,
        })
    }
}
