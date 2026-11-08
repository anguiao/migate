use super::{AuthError, AuthService, CertificateUpdate, service::verify_account_uid};
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
        verify_account_uid(&record.uid, home.uid.as_deref()).map_err(AuthError::Failure)
    }

    pub(crate) async fn refresh_bridge_tokens(
        &self,
        record: &XiaomiRecord,
        now: impl Fn() -> i64,
    ) -> Result<XiaomiRecord, AuthError> {
        let authorization = record
            .ha_authorization()
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        self.refresh_tokens_once(&authorization.redirect_uri, &authorization.tokens, now)
            .await?;
        Ok(self
            .store
            .current_snapshot()
            .record
            .expect("credentials saved before reload"))
    }

    pub(crate) async fn renew_bridge_certificate(
        &self,
        record: &XiaomiRecord,
        generation: u64,
        now: impl Fn() -> i64,
    ) -> Result<super::BridgeCertificate, AuthError> {
        let mut record = record.clone();
        let identity = self.prepare_client_identity(&mut record)?;
        let authorization = record
            .ha_authorization()
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        let certificate = self
            .cloud
            .get_certificate(&authorization.tokens.access_token, &identity.csr_pem)
            .await;
        if !self.store.current_ha(&record.uid, generation) {
            return Err(AuthError::Missing(
                "HA OAuth credentials changed during certificate maintenance",
            ));
        }
        let certificate = certificate.map_err(AuthError::Cloud)?;
        let update = match self.save_certificate_response(&record, &certificate, now())? {
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
