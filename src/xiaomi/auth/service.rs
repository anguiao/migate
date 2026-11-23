use super::{
    AccountClient, AccountIdentity, AuthError, AuthReport, AuthenticationState, CertificateUpdate,
    FailureReason, refresh::require_ha,
};
use crate::{
    storage::{HubIdentity, StorageError, XiaomiRecord, XiaomiStore},
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

    pub(in crate::xiaomi) fn mijia_client(&self) -> MijiaClient {
        self.mijia.clone()
    }

    pub(crate) fn logout(&self) -> Result<(), AuthError> {
        self.store.logout()?;
        Ok(())
    }

    pub fn local_status(&self) -> Result<AuthReport, AuthError> {
        let stored = self.load_validated()?;
        Ok(self.report_for(stored.as_ref()))
    }

    pub(super) fn report_for(&self, stored: Option<&StoredRecord>) -> AuthReport {
        match stored {
            Some(stored) => AuthReport {
                account: Some(AccountIdentity {
                    uid: stored.record.uid.clone(),
                    region: stored.record.region.clone(),
                }),
                recovery_stop: stored.record.recovery.stop,
                mijia: AuthenticationState::Checking,
                ha: if stored.record.ha.stopped {
                    AuthenticationState::SignInRequired(FailureReason::HaAuthorizationStopped)
                } else {
                    AuthenticationState::Checking
                },
                certificate: stored.validity,
                certificate_update: stored.certificate_update.clone(),
                login_failure: None,
                completed_at: (self.now)(),
            },
            None => AuthReport::not_signed_in((self.now)()),
        }
    }

    /// Saved local credentials remain useful when cloud preparation is pending
    /// or unavailable. Gateway TLS performs current validity and authorization.
    pub fn saved_bridge_credentials(&self) -> Result<Option<XiaomiRecord>, AuthError> {
        Ok(self.store.snapshot()?.record)
    }

    pub(super) fn prepare_client_identity(
        &self,
        uid: &str,
        generation: u64,
    ) -> Result<(XiaomiRecord, ClientIdentity), AuthError> {
        require_ha(&self.store, uid, generation)?;
        let mut record = self
            .store
            .current_snapshot()
            .record
            .expect("current HA authorization");
        let identity = if let Some(hub) = &record.hub {
            ClientIdentity::from_private_key(&record.uid, &hub.virtual_did, &hub.private_key_pem)
                .map_err(|error| AuthError::Storage(self.corrupt_record(error)))?
        } else {
            let identity = ClientIdentity::generate(&record.uid)
                .map_err(|_| AuthError::Failure(FailureReason::InvalidClientIdentity))?;
            let hub = HubIdentity {
                virtual_did: identity.virtual_did.clone(),
                private_key_pem: identity.private_key_pem.clone(),
                certificate_pem: None,
            };
            self.store.create_hub(&hub)?;
            record.hub = Some(hub);
            identity
        };
        Ok((record, identity))
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
        self.load_validated()?
            .ok_or(AuthError::Missing("Xiaomi account is unavailable"))
    }

    pub(super) fn load_validated(&self) -> Result<Option<StoredRecord>, AuthError> {
        let snapshot = self.store.snapshot()?;
        let Some(record) = snapshot.record else {
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
            login_generation: snapshot.login_generation,
            mijia_generation: snapshot.mijia_generation,
            ha_generation: snapshot.ha_generation,
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

pub(super) fn verify_account_uid(
    expected: &str,
    actual: Option<&str>,
) -> Result<(), FailureReason> {
    match actual {
        Some(uid) if uid == expected => Ok(()),
        Some(_) => Err(FailureReason::AccountIdentityMismatch),
        None => Err(FailureReason::CannotDetermineAccountIdentity),
    }
}

fn current_time() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub(super) struct StoredRecord {
    pub(super) record: XiaomiRecord,
    pub(super) login_generation: u64,
    pub(super) mijia_generation: u64,
    pub(super) ha_generation: u64,
    pub(super) validity: Option<CertificateValidity>,
    pub(super) certificate_update: CertificateUpdate,
}
