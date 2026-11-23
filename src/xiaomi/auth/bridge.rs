use super::{AuthError, AuthService, CertificateUpdate, refresh::require_ha};
use crate::{storage::XiaomiRecord, xiaomi::certificate::CertificateValidity};

#[derive(Debug)]
pub(crate) struct BridgeCertificate {
    pub(crate) credentials: XiaomiRecord,
    pub(crate) validity: Option<CertificateValidity>,
    pub(crate) update: CertificateUpdate,
}

impl AuthService {
    pub(crate) async fn refresh_bridge_tokens(
        &self,
        record: &XiaomiRecord,
        generation: u64,
        now: impl FnOnce() -> i64 + 'static,
    ) -> Result<XiaomiRecord, AuthError> {
        let authorization = record
            .ha_authorization()
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        self.refresh_tokens_once(&record.uid, generation, authorization, now)
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
        let (record, identity) = self.prepare_client_identity(&record.uid, generation)?;
        let authorization = record
            .ha_authorization()
            .ok_or(AuthError::Missing("HA OAuth credentials are unavailable"))?;
        let certificate = self
            .request_certificate(
                &record,
                &authorization.tokens.access_token,
                &identity.csr_pem,
            )
            .await;
        require_ha(&self.store, &record.uid, generation)?;
        let update = match certificate? {
            None => CertificateUpdate::NotNeeded,
            Some(certificate) => {
                match self.save_certificate_response(&record, &certificate, now())? {
                    Ok(_) => CertificateUpdate::Updated,
                    Err(reason) => CertificateUpdate::Failed(reason),
                }
            }
        };
        let stored = self.reload_credentials()?;
        Ok(BridgeCertificate {
            credentials: stored.record,
            validity: stored.validity,
            update,
        })
    }
}
