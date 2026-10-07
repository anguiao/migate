use super::{AuthError, AuthReport, AuthenticationState, CertificateUpdate, FailureReason, tokens};
use crate::{
    storage::{StorageError, TokenSet, XiaomiRecord, XiaomiStore},
    xiaomi::{
        certificate::{CertificateValidity, ClientIdentity, validate_certificate},
        cloud::{CloudClient, OwnedHome, validate_saved_redirect_uri},
    },
};
use std::error::Error as StdError;
use time::OffsetDateTime;

pub struct AuthService {
    pub(super) store: XiaomiStore,
    pub(super) cloud: CloudClient,
    pub(super) now: fn() -> i64,
}

#[derive(Debug)]
pub struct BridgePreparation {
    pub credentials: Option<XiaomiRecord>,
    pub report: AuthReport,
    pub homes: Vec<OwnedHome>,
}

impl AuthService {
    pub fn new(store: XiaomiStore, cloud: CloudClient) -> Self {
        Self {
            store,
            cloud,
            now: current_time,
        }
    }

    pub fn local_status(&self) -> Result<AuthReport, AuthError> {
        let status = self.load_validated()?;
        Ok(match status {
            Some(stored) => AuthReport {
                authentication: AuthenticationState::Checking,
                certificate: Some(stored.validity),
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            },
            None => AuthReport {
                authentication: AuthenticationState::NotSignedIn,
                certificate: None,
                certificate_update: CertificateUpdate::NotNeeded,
                completed_at: (self.now)(),
            },
        })
    }

    pub async fn check(&self) -> Result<AuthReport, AuthError> {
        self.check_mode(None).await
    }

    /// Prepare authentication without the standalone all-home device probe.
    /// A cloud failure remains diagnostic; local authorization is determined
    /// by the gateway using these saved credentials. Source owns static cache restoration.
    pub async fn prepare_bridge(&self) -> Result<BridgePreparation, AuthError> {
        let mut homes = Vec::new();
        let report = self.check_mode(Some(&mut homes)).await?;
        if matches!(
            report.authentication,
            AuthenticationState::Unavailable(
                FailureReason::AccountIdentityMismatch
                    | FailureReason::CannotDetermineAccountIdentity
            )
        ) {
            homes.clear();
        }
        let credentials = self.load_validated()?.map(|stored| stored.record);
        Ok(BridgePreparation {
            credentials,
            report,
            homes,
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
        refresh_token: &str,
        now: impl Fn() -> i64,
    ) -> Result<TokenSet, AuthError> {
        let response = self
            .cloud
            .refresh_token(redirect_uri, refresh_token)
            .await
            .map_err(AuthError::Cloud)?;
        let replacement = tokens::from_response(response, now()).map_err(AuthError::Cloud)?;
        self.store.update_tokens(&replacement)?;
        Ok(replacement)
    }

    pub(super) fn client_identity(
        &self,
        record: &XiaomiRecord,
    ) -> Result<ClientIdentity, AuthError> {
        ClientIdentity::from_private_key(&record.uid, &record.virtual_did, &record.private_key_pem)
            .map_err(|error| self.corrupt_record(error).into())
    }

    pub(super) fn save_certificate_response(
        &self,
        record: &XiaomiRecord,
        certificate_pem: &str,
        completed_at: i64,
    ) -> Result<Result<CertificateValidity, FailureReason>, AuthError> {
        let validity = match validate_certificate(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
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
        let Some(record) = self.store.load().map_err(AuthError::Storage)? else {
            return Ok(None);
        };
        validate_saved_redirect_uri(&record.redirect_uri)
            .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?;
        let validity = validate_certificate(
            &record.uid,
            &record.virtual_did,
            &record.private_key_pem,
            &record.certificate_pem,
        )
        .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?;
        Ok(Some(StoredRecord { record, validity }))
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
    pub(super) validity: CertificateValidity,
}
