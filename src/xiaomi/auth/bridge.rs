use super::{AuthError, AuthService, CertificateUpdate};
use crate::{storage::XiaomiRecord, xiaomi::certificate::CertificateValidity};

#[derive(Debug)]
pub(crate) struct BridgeCertificate {
    pub(crate) credentials: XiaomiRecord,
    pub(crate) validity: CertificateValidity,
    pub(crate) update: CertificateUpdate,
}

impl AuthService {
    pub(crate) async fn refresh_bridge_tokens(
        &self,
        record: &XiaomiRecord,
        now: impl Fn() -> i64,
    ) -> Result<XiaomiRecord, AuthError> {
        self.refresh_tokens_once(&record.redirect_uri, &record.tokens.refresh_token, now)
            .await?;
        Ok(self.reload_credentials()?.record)
    }

    pub(crate) async fn renew_bridge_certificate(
        &self,
        record: &XiaomiRecord,
        now: impl Fn() -> i64,
    ) -> Result<super::BridgeCertificate, AuthError> {
        let identity = self.client_identity(record)?;
        let certificate = self
            .cloud
            .get_certificate(&record.tokens.access_token, &identity.csr_pem)
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
