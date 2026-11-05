use super::{
    AccountClient, AccountIdentity, AuthError, AuthReport, AuthenticationState, CertificateUpdate,
    FailureReason, tokens,
};
use crate::{
    storage::{StorageError, TokenSet, XiaomiRecord, XiaomiStore},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity, validate_certificate},
        cloud::{CloudClient, CloudError},
        mijia::MijiaClient,
    },
};
use std::error::Error as StdError;
use time::OffsetDateTime;

pub struct AuthService {
    pub(super) store: XiaomiStore,
    pub(super) cloud: CloudClient,
    pub(super) account: AccountClient,
    pub(super) mijia: MijiaClient,
    pub(super) now: fn() -> i64,
}

impl AuthService {
    pub fn new(store: XiaomiStore, cloud: CloudClient) -> Result<Self, CloudError> {
        Ok(Self {
            store,
            cloud,
            account: AccountClient::new(),
            mijia: MijiaClient::new()?,
            now: current_time,
        })
    }

    pub(crate) fn mijia_client(&self) -> MijiaClient {
        self.mijia.clone()
    }

    #[cfg(test)]
    pub(in crate::xiaomi) fn for_adapter_test(
        store: XiaomiStore,
        cloud: CloudClient,
        base: &str,
    ) -> Self {
        Self {
            store,
            cloud,
            account: AccountClient::for_test(base, std::time::Duration::from_secs(1)),
            mijia: MijiaClient::for_test(base, std::time::Duration::from_secs(1)),
            now: current_time,
        }
    }

    pub fn local_status(&self) -> Result<AuthReport, AuthError> {
        let status = self.load_validated()?;
        Ok(match status {
            Some(stored) => AuthReport {
                account: Some(AccountIdentity {
                    uid: stored.record.uid.clone(),
                    region: stored.record.region.clone(),
                }),
                recovery_stop: stored
                    .record
                    .recovery
                    .as_ref()
                    .and_then(|recovery| recovery.stop),
                mijia: if stored.record.mijia.is_some() {
                    AuthenticationState::Checking
                } else if let Some(stop) = stored
                    .record
                    .recovery
                    .as_ref()
                    .and_then(|recovery| recovery.stop)
                {
                    AuthenticationState::SignInRequired(FailureReason::RecoveryStopped(stop))
                } else {
                    AuthenticationState::Unavailable(FailureReason::MissingMijiaSession)
                },
                ha: match &stored.record.ha {
                    Some(ha) if ha.stopped => {
                        AuthenticationState::SignInRequired(FailureReason::MissingHaAuthorization)
                    }
                    Some(ha) if ha.authorization.is_some() => AuthenticationState::Checking,
                    _ => AuthenticationState::SignInRequired(FailureReason::MissingHaAuthorization),
                },
                certificate: stored.validity,
                certificate_update: stored.certificate_update,
                login_failure: None,
                completed_at: (self.now)(),
            },
            None => AuthReport {
                account: None,
                recovery_stop: None,
                mijia: AuthenticationState::NotSignedIn,
                ha: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                login_failure: None,
                completed_at: (self.now)(),
            },
        })
    }

    /// Saved local credentials remain useful when cloud preparation is pending
    /// or unavailable. Gateway TLS performs current validity and authorization.
    pub fn saved_bridge_credentials(&self) -> Result<Option<XiaomiRecord>, AuthError> {
        Ok(self.load_validated()?.map(|stored| stored.record))
    }

    pub(super) async fn refresh_tokens_once(
        &self,
        redirect_uri: &str,
        tokens: &TokenSet,
        now: impl Fn() -> i64,
    ) -> Result<TokenSet, AuthError> {
        let response = self
            .cloud
            .refresh_token(redirect_uri, &tokens.refresh_token)
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if error.is_unauthorized() {
                    self.store.stop_ha_refresh()?;
                }
                return Err(AuthError::Cloud(error));
            }
        };
        let replacement = tokens::from_response(response, now()).map_err(AuthError::Cloud)?;
        self.store.update_tokens(&replacement)?;
        Ok(replacement)
    }

    pub(super) fn client_identity(
        &self,
        record: &XiaomiRecord,
    ) -> Result<ClientIdentity, AuthError> {
        let hub = record
            .hub
            .as_ref()
            .ok_or(AuthError::Missing("Gateway identity is unavailable"))?;
        ClientIdentity::from_private_key(&record.uid, &hub.virtual_did, &hub.private_key_pem)
            .map_err(|error| self.corrupt_record(error).into())
    }

    pub(super) fn save_certificate_response(
        &self,
        record: &XiaomiRecord,
        certificate_pem: &str,
        completed_at: i64,
    ) -> Result<Result<CertificateValidity, FailureReason>, AuthError> {
        let hub = record
            .hub
            .as_ref()
            .ok_or(AuthError::Missing("Gateway identity is unavailable"))?;
        let validity = match validate_certificate(
            &record.uid,
            &hub.virtual_did,
            &hub.private_key_pem,
            certificate_pem,
        ) {
            Ok(validity) => validity,
            Err(_) => return Ok(Err(FailureReason::InvalidCertificate)),
        };
        if !validity.currently_valid(completed_at) {
            return Ok(Err(FailureReason::CertificateNotCurrentlyValid));
        }
        self.store.update_certificate(certificate_pem)?;
        Ok(Ok(validity))
    }

    pub(super) fn reload_credentials(&self) -> Result<StoredRecord, AuthError> {
        self.load_validated()?.ok_or_else(|| {
            self.corrupt_record("Xiaomi credentials disappeared after an update")
                .into()
        })
    }

    pub(super) fn load_validated(&self) -> Result<Option<StoredRecord>, AuthError> {
        let Some(record) = self.store.snapshot().map_err(AuthError::Storage)?.record else {
            return Ok(None);
        };
        let (validity, certificate_update) = if let Some(hub) = &record.hub {
            ClientIdentity::from_private_key(&record.uid, &hub.virtual_did, &hub.private_key_pem)
                .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?;
            if let Some(certificate) = hub.certificate_pem.as_deref() {
                match validate_certificate(
                    &record.uid,
                    &hub.virtual_did,
                    &hub.private_key_pem,
                    certificate,
                ) {
                    Ok(validity) => (Some(validity), CertificateUpdate::NotNeeded),
                    Err(_) => (
                        None,
                        CertificateUpdate::Failed(FailureReason::InvalidCertificate),
                    ),
                }
            } else {
                (
                    None,
                    CertificateUpdate::Failed(FailureReason::MissingCertificate),
                )
            }
        } else {
            (
                None,
                CertificateUpdate::Failed(FailureReason::MissingCertificate),
            )
        };
        Ok(Some(StoredRecord {
            record,
            validity,
            certificate_update,
        }))
    }

    pub(super) fn corrupt_record(&self, source: impl Into<Box<dyn StdError>>) -> StorageError {
        StorageError::new(
            self.store.path(),
            "validate Xiaomi authentication credentials",
            source,
        )
    }
}

fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub(super) struct StoredRecord {
    pub(super) record: XiaomiRecord,
    pub(super) validity: Option<CertificateValidity>,
    pub(super) certificate_update: CertificateUpdate,
}
