use super::{AuthError, AuthReport, AuthenticationState, CertificateUpdate, FailureReason};
use crate::{
    device::DeviceKey,
    storage::{CachedDevice, StorageError, XiaomiRecord, XiaomiStore},
    xiaomi::{
        certificate::{CertificateValidity, validate_certificate},
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
    pub cached_device: Option<CachedDevice>,
}

impl AuthService {
    pub fn new(store: XiaomiStore, cloud: CloudClient) -> Self {
        Self {
            store,
            cloud,
            now: current_time,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(store: XiaomiStore, cloud: CloudClient, now: fn() -> i64) -> Self {
        Self { store, cloud, now }
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
    /// by the gateway using these saved credentials and same-account cache.
    pub async fn prepare_bridge(&self, key: &DeviceKey) -> Result<BridgePreparation, AuthError> {
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
        let cached_device = match &credentials {
            Some(record) => self.store.devices().load(&record.uid, key)?,
            None => None,
        };
        Ok(BridgePreparation {
            credentials,
            report,
            homes,
            cached_device,
        })
    }

    /// Saved local credentials remain useful when cloud preparation is pending
    /// or unavailable. Gateway TLS performs current validity and authorization.
    pub fn saved_bridge_credentials(&self) -> Result<Option<XiaomiRecord>, AuthError> {
        Ok(self.load_validated()?.map(|stored| stored.record))
    }

    /// Load the current account's static topology before network preparation.
    pub fn load_bridge_cache(&self, key: &DeviceKey) -> Result<Option<CachedDevice>, AuthError> {
        let Some(stored) = self.load_validated()? else {
            return Ok(None);
        };
        self.store
            .devices()
            .load(&stored.record.uid, key)
            .map_err(AuthError::Storage)
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
