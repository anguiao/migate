use super::{StorageError, Store};
use crate::xiaomi::{
    auth::{AccountClientIdentity, AccountCredentials},
    cloud::validate_saved_redirect_uri,
    mijia::MijiaSession,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{error::Error as StdError, fmt, path::Path};
use uuid::Uuid;

#[derive(Clone, Eq, PartialEq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub refresh_at: i64,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("refresh_at", &self.refresh_at)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryStop {
    Rejected,
    VerificationRequired,
}

impl RecoveryStop {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::VerificationRequired => "verification_required",
        }
    }

    fn parse(value: &str) -> Result<Self, InvalidCredentials> {
        match value {
            "rejected" => Ok(Self::Rejected),
            "verification_required" => Ok(Self::VerificationRequired),
            _ => Err(InvalidCredentials),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountRecovery {
    pub credentials: AccountCredentials,
    pub stop: Option<RecoveryStop>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct HaAuthorization {
    pub redirect_uri: String,
    pub tokens: TokenSet,
}

impl fmt::Debug for HaAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HaAuthorization")
            .field("redirect_uri", &"[REDACTED]")
            .field("tokens", &self.tokens)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HaClient {
    pub client_uuid: String,
    pub authorization: Option<HaAuthorization>,
    pub stopped: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub struct HubIdentity {
    pub virtual_did: String,
    pub private_key_pem: String,
    pub certificate_pem: Option<String>,
}

impl fmt::Debug for HubIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HubIdentity")
            .field("virtual_did", &self.virtual_did)
            .field("private_key_pem", &"[REDACTED]")
            .field(
                "certificate_pem",
                &self.certificate_pem.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiRecord {
    pub uid: String,
    pub region: String,
    pub recovery: Option<AccountRecovery>,
    pub mijia: Option<MijiaSession>,
    pub ha: Option<HaClient>,
    pub hub: Option<HubIdentity>,
}

impl XiaomiRecord {
    pub fn ha_authorization(&self) -> Option<&HaAuthorization> {
        self.ha
            .as_ref()
            .filter(|ha| !ha.stopped)?
            .authorization
            .as_ref()
    }
}

/// Process-local identities for asynchronous operations using saved credentials.
#[derive(Clone)]
pub(crate) struct CredentialSnapshot {
    pub(crate) record: Option<XiaomiRecord>,
    pub(crate) mijia_generation: u64,
    pub(crate) ha_generation: u64,
}

impl CredentialSnapshot {
    pub(crate) fn mijia_generation(&self) -> Option<u64> {
        self.record.as_ref().map(|_| self.mijia_generation)
    }

    pub(crate) fn ha_generation(&self) -> Option<u64> {
        self.record.as_ref()?.ha_authorization()?;
        Some(self.ha_generation)
    }
}

#[derive(Clone)]
pub struct XiaomiStore {
    store: Store,
}

impl XiaomiStore {
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn path(&self) -> &Path {
        self.store.path()
    }

    pub fn load(&self) -> Result<Option<XiaomiRecord>, StorageError> {
        self.load_inner().map_err(|error| match error {
            LoadError::Database(error) => {
                self.store.database_error("load Xiaomi credentials", error)
            }
            LoadError::Invalid => StorageError::new(
                self.path(),
                "validate Xiaomi credentials",
                InvalidCredentials,
            ),
        })
    }

    /// Initialize the runtime snapshot at the storage boundary once per Store.
    pub(crate) fn snapshot(&self) -> Result<CredentialSnapshot, StorageError> {
        if self.store.xiaomi_state().borrow().is_none() {
            let record = self.load()?;
            *self.store.xiaomi_state().borrow_mut() = Some(CredentialSnapshot {
                record,
                mijia_generation: 0,
                ha_generation: 0,
            });
        }
        Ok(self.current_snapshot())
    }

    pub(crate) fn current_snapshot(&self) -> CredentialSnapshot {
        self.store
            .xiaomi_state()
            .borrow()
            .as_ref()
            .expect("credentials initialized")
            .clone()
    }

    pub(crate) fn current_mijia(&self, uid: &str, generation: u64) -> bool {
        self.store
            .xiaomi_state()
            .borrow()
            .as_ref()
            .is_some_and(|state| {
                state.mijia_generation() == Some(generation)
                    && state
                        .record
                        .as_ref()
                        .is_some_and(|record| record.uid == uid)
            })
    }

    pub(crate) fn current_ha(&self, uid: &str, generation: u64) -> bool {
        self.store
            .xiaomi_state()
            .borrow()
            .as_ref()
            .is_some_and(|state| {
                state.ha_generation() == Some(generation)
                    && state
                        .record
                        .as_ref()
                        .is_some_and(|record| record.uid == uid)
            })
    }

    fn load_inner(&self) -> Result<Option<XiaomiRecord>, LoadError> {
        let db = self.store.connection();
        let invalid_slot: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM xiaomi_account WHERE id <> 1)
                 OR EXISTS(SELECT 1 FROM xiaomi_account_recovery WHERE id <> 1)
                 OR EXISTS(SELECT 1 FROM xiaomi_mijia_session WHERE id <> 1)
                 OR EXISTS(SELECT 1 FROM xiaomi_ha_client WHERE id <> 1)
                 OR EXISTS(SELECT 1 FROM xiaomi_ha_oauth WHERE id <> 1)
                 OR EXISTS(SELECT 1 FROM xiaomi_hub WHERE id <> 1)",
            [],
            |row| row.get(0),
        )?;
        if invalid_slot {
            return Err(LoadError::Invalid);
        }
        let account: Option<(String, String)> = db
            .query_row(
                "SELECT uid, region FROM xiaomi_account WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let recovery_row: Option<(String, String, String, String, String, Option<String>)> = db.query_row(
            "SELECT user_id, c_user_id, pass_token, device_id, user_agent, stopped FROM xiaomi_account_recovery WHERE id = 1", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
        let session_row: Option<(String, String, String, String)> = db.query_row(
            "SELECT user_id, service_token, ssecurity, user_agent FROM xiaomi_mijia_session WHERE id = 1", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let client_row: Option<(String, bool)> = db
            .query_row(
                "SELECT client_uuid, stopped FROM xiaomi_ha_client WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let authorization: Option<HaAuthorization> = db.query_row(
            "SELECT redirect_uri, access_token, refresh_token, expires_at, refresh_at FROM xiaomi_ha_oauth WHERE id = 1", [],
            |row| Ok(HaAuthorization { redirect_uri: row.get(0)?, tokens: TokenSet { access_token: row.get(1)?, refresh_token: row.get(2)?, expires_at: row.get(3)?, refresh_at: row.get(4)? } }),
        ).optional()?;
        let hub: Option<HubIdentity> = db
            .query_row(
                "SELECT virtual_did, private_key_pem, certificate_pem FROM xiaomi_hub WHERE id = 1",
                [],
                |row| {
                    Ok(HubIdentity {
                        virtual_did: row.get(0)?,
                        private_key_pem: row.get(1)?,
                        certificate_pem: row.get(2)?,
                    })
                },
            )
            .optional()?;
        let Some((uid, region)) = account else {
            if recovery_row.is_some()
                || session_row.is_some()
                || client_row.is_some()
                || authorization.is_some()
                || hub.is_some()
            {
                return Err(LoadError::Invalid);
            }
            return Ok(None);
        };
        let recovery = recovery_row
            .map(
                |(user_id, c_user_id, pass_token, device_id, user_agent, stop)| {
                    let identity = AccountClientIdentity::new(device_id, user_agent)
                        .map_err(|_| LoadError::Invalid)?;
                    let credentials =
                        AccountCredentials::new(user_id, c_user_id, pass_token, identity)
                            .map_err(|_| LoadError::Invalid)?;
                    let stop = stop
                        .as_deref()
                        .map(RecoveryStop::parse)
                        .transpose()
                        .map_err(|_| LoadError::Invalid)?;
                    Ok::<_, LoadError>(AccountRecovery { credentials, stop })
                },
            )
            .transpose()?;
        let mijia = session_row
            .map(|(user_id, service_token, ssecurity, user_agent)| {
                MijiaSession::new(user_id, service_token, ssecurity, user_agent)
                    .map_err(|_| LoadError::Invalid)
            })
            .transpose()?;
        let ha = match (client_row, authorization) {
            (Some((client_uuid, stopped)), authorization) => Some(HaClient {
                client_uuid,
                authorization,
                stopped,
            }),
            (None, None) => None,
            (None, Some(_)) => return Err(LoadError::Invalid),
        };
        let record = XiaomiRecord {
            uid,
            region,
            recovery,
            mijia,
            ha,
            hub,
        };
        validate_record(&record).map_err(|_| LoadError::Invalid)?;
        Ok(Some(record))
    }

    /// Commit all credentials obtained during one login while preserving the
    /// existing hub identity. This is the only maintenance operation that
    /// clears both durable stop flags.
    /// The login flow has already verified the account and retained the client UUID.
    pub(crate) fn commit_login(
        &self,
        credentials: &AccountCredentials,
        session: &MijiaSession,
        client_uuid: &str,
        authorization: &HaAuthorization,
    ) -> Result<(), StorageError> {
        let candidate = XiaomiRecord {
            uid: credentials.user_id().to_owned(),
            region: "cn".into(),
            recovery: Some(AccountRecovery {
                credentials: credentials.clone(),
                stop: None,
            }),
            mijia: Some(session.clone()),
            ha: Some(HaClient {
                client_uuid: client_uuid.to_owned(),
                authorization: Some(authorization.clone()),
                stopped: false,
            }),
            hub: None,
        };
        self.validate(&candidate)?;
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            tx.execute("INSERT INTO xiaomi_account (id, uid, region) VALUES (1, ?1, 'cn') ON CONFLICT(id) DO NOTHING", [&candidate.uid])?;
            write_recovery(&tx, credentials, None)?;
            write_session(&tx, session)?;
            write_ha(&tx, candidate.ha.as_ref().expect("candidate includes HA client"))?;
            tx.commit()
        })().map_err(|error| self.store.database_error("commit Xiaomi login", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            let mut candidate = candidate;
            candidate.hub = state.record.as_ref().and_then(|record| record.hub.clone());
            state.record = Some(candidate);
            state.mijia_generation = state.mijia_generation.wrapping_add(1);
            state.ha_generation = state.ha_generation.wrapping_add(1);
        }
        Ok(())
    }

    pub(crate) fn update_recovery(
        &self,
        credentials: &AccountCredentials,
        session: &MijiaSession,
    ) -> Result<(), StorageError> {
        if credentials.user_id() != session.user_id() {
            return Err(StorageError::new(
                self.path(),
                "validate Xiaomi recovery",
                InvalidCredentials,
            ));
        }
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            write_recovery(&tx, credentials, None)?;
            write_session(&tx, session)?;
            tx.commit()
        })()
        .map_err(|error| self.store.database_error("update Xiaomi recovery", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            let record = state.record.as_mut().expect("initialized account");
            record.recovery = Some(AccountRecovery {
                credentials: credentials.clone(),
                stop: None,
            });
            record.mijia = Some(session.clone());
            state.mijia_generation = state.mijia_generation.wrapping_add(1);
        }
        Ok(())
    }

    pub(crate) fn stop_recovery(&self, stop: RecoveryStop) -> Result<(), StorageError> {
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_account_recovery SET stopped = ?1 WHERE id = 1",
                [stop.as_str()],
            )
            .map_err(|error| self.store.database_error("stop Xiaomi recovery", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state
                .record
                .as_mut()
                .expect("initialized account")
                .recovery
                .as_mut()
                .expect("recovery credentials")
                .stop = Some(stop);
        }
        Ok(())
    }

    pub(crate) fn stop_ha_refresh(&self) -> Result<(), StorageError> {
        self.store
            .connection()
            .execute("UPDATE xiaomi_ha_client SET stopped = 1 WHERE id = 1", [])
            .map_err(|error| self.store.database_error("stop HA refresh", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state
                .record
                .as_mut()
                .expect("initialized account")
                .ha
                .as_mut()
                .expect("HA client")
                .stopped = true;
        }
        Ok(())
    }

    pub(crate) fn update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_ha_oauth SET access_token = ?1, refresh_token = ?2,
             expires_at = ?3, refresh_at = ?4 WHERE id = 1",
                (
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                ),
            )
            .map_err(|error| self.store.database_error("update Xiaomi tokens", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state
                .record
                .as_mut()
                .expect("initialized account")
                .ha
                .as_mut()
                .expect("HA client")
                .authorization
                .as_mut()
                .expect("HA authorization")
                .tokens = tokens.clone();
            state.ha_generation = state.ha_generation.wrapping_add(1);
        }
        Ok(())
    }

    pub(crate) fn create_hub(&self, hub: &HubIdentity) -> Result<(), StorageError> {
        validate_hub(hub).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi hub identity", error)
        })?;
        self.store.connection().execute(
            "INSERT INTO xiaomi_hub (id, virtual_did, private_key_pem, certificate_pem) VALUES (1, ?1, ?2, ?3)",
            (&hub.virtual_did, &hub.private_key_pem, &hub.certificate_pem),
        ).map_err(|error| self.store.database_error("create Xiaomi hub identity", error))?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state.record.as_mut().expect("initialized account").hub = Some(hub.clone());
        }
        Ok(())
    }

    pub(crate) fn update_certificate(&self, certificate_pem: &str) -> Result<(), StorageError> {
        validate_pem(certificate_pem).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi certificate", error)
        })?;
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_hub SET certificate_pem = ?1 WHERE id = 1",
                [certificate_pem],
            )
            .map_err(|error| {
                self.store
                    .database_error("update Xiaomi certificate", error)
            })?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state
                .record
                .as_mut()
                .expect("initialized account")
                .hub
                .as_mut()
                .expect("hub identity")
                .certificate_pem = Some(certificate_pem.to_owned());
        }
        Ok(())
    }

    pub fn logout(&self) -> Result<(), StorageError> {
        (|| {
            let transaction = self.store.connection().unchecked_transaction()?;
            for table in [
                "xiaomi_lan",
                "xiaomi_account_recovery",
                "xiaomi_mijia_session",
                "xiaomi_ha_oauth",
                "xiaomi_ha_client",
                "xiaomi_hub",
                "xiaomi_account",
            ] {
                transaction.execute(&format!("DELETE FROM {table}"), [])?;
            }
            transaction.commit()
        })()
        .map_err(|error| {
            self.store
                .database_error("delete Xiaomi credentials", error)
        })?;
        if let Some(state) = self.store.xiaomi_state().borrow_mut().as_mut() {
            state.record = None;
            state.mijia_generation = state.mijia_generation.wrapping_add(1);
            state.ha_generation = state.ha_generation.wrapping_add(1);
        }
        Ok(())
    }

    fn validate(&self, record: &XiaomiRecord) -> Result<(), StorageError> {
        validate_record(record)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi credentials", error))
    }
}

fn write_recovery(
    db: &Connection,
    credentials: &AccountCredentials,
    stop: Option<RecoveryStop>,
) -> rusqlite::Result<()> {
    db.execute("INSERT INTO xiaomi_account_recovery (id, user_id, c_user_id, pass_token, device_id, user_agent, stopped) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(id) DO UPDATE SET user_id=excluded.user_id, c_user_id=excluded.c_user_id, pass_token=excluded.pass_token, device_id=excluded.device_id, user_agent=excluded.user_agent, stopped=excluded.stopped",
        params![credentials.user_id(), credentials.c_user_id(), credentials.pass_token(), credentials.identity().device_id(), credentials.identity().user_agent(), stop.map(RecoveryStop::as_str)])?;
    Ok(())
}

fn write_session(db: &Connection, session: &MijiaSession) -> rusqlite::Result<()> {
    db.execute("INSERT INTO xiaomi_mijia_session (id, user_id, service_token, ssecurity, user_agent) VALUES (1, ?1, ?2, ?3, ?4)
        ON CONFLICT(id) DO UPDATE SET user_id=excluded.user_id, service_token=excluded.service_token, ssecurity=excluded.ssecurity, user_agent=excluded.user_agent",
        params![session.user_id(), session.service_token(), session.ssecurity(), session.user_agent()])?;
    Ok(())
}

fn write_ha(db: &Connection, ha: &HaClient) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO xiaomi_ha_client (id, client_uuid, stopped) VALUES (1, ?1, ?2)
        ON CONFLICT(id) DO UPDATE SET client_uuid=excluded.client_uuid, stopped=excluded.stopped",
        (&ha.client_uuid, ha.stopped),
    )?;
    if let Some(authorization) = &ha.authorization {
        let tokens = &authorization.tokens;
        db.execute("INSERT INTO xiaomi_ha_oauth (id, redirect_uri, access_token, refresh_token, expires_at, refresh_at) VALUES (1, ?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(id) DO UPDATE SET redirect_uri=excluded.redirect_uri, access_token=excluded.access_token, refresh_token=excluded.refresh_token, expires_at=excluded.expires_at, refresh_at=excluded.refresh_at",
            (&authorization.redirect_uri, &tokens.access_token, &tokens.refresh_token, tokens.expires_at, tokens.refresh_at))?;
    }
    Ok(())
}

#[derive(Debug)]
struct InvalidCredentials;
impl fmt::Display for InvalidCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Stored Xiaomi credentials are invalid")
    }
}
impl StdError for InvalidCredentials {}

enum LoadError {
    Database(rusqlite::Error),
    Invalid,
}
impl From<rusqlite::Error> for LoadError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

fn validate_record(record: &XiaomiRecord) -> Result<(), InvalidCredentials> {
    validate_text(&record.uid)?;
    if record.region != "cn" {
        return Err(InvalidCredentials);
    }
    if let Some(recovery) = &record.recovery
        && recovery.credentials.user_id() != record.uid
    {
        return Err(InvalidCredentials);
    }
    if let Some(session) = &record.mijia
        && session.user_id() != record.uid
    {
        return Err(InvalidCredentials);
    }
    if let Some(ha) = &record.ha {
        validate_uuid(&ha.client_uuid)?;
        if let Some(authorization) = &ha.authorization {
            validate_saved_redirect_uri(&authorization.redirect_uri)
                .map_err(|_| InvalidCredentials)?;
            validate_tokens(&authorization.tokens)?;
        }
    }
    if let Some(hub) = &record.hub {
        validate_hub(hub)?;
    }
    Ok(())
}

fn validate_hub(hub: &HubIdentity) -> Result<(), InvalidCredentials> {
    validate_virtual_did(&hub.virtual_did)?;
    validate_pem(&hub.private_key_pem)?;
    if let Some(cert) = &hub.certificate_pem {
        validate_pem(cert)?;
    }
    Ok(())
}
fn validate_tokens(tokens: &TokenSet) -> Result<(), InvalidCredentials> {
    validate_text(&tokens.access_token)?;
    validate_text(&tokens.refresh_token)?;
    if tokens.access_token.chars().any(char::is_whitespace)
        || tokens.refresh_token.chars().any(char::is_whitespace)
    {
        return Err(InvalidCredentials);
    }
    if tokens.refresh_at <= 0 || tokens.expires_at <= 0 || tokens.refresh_at > tokens.expires_at {
        return Err(InvalidCredentials);
    }
    Ok(())
}
fn validate_uuid(value: &str) -> Result<(), InvalidCredentials> {
    let uuid = Uuid::parse_str(value).map_err(|_| InvalidCredentials)?;
    if uuid.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(InvalidCredentials)
    }
}
fn validate_virtual_did(value: &str) -> Result<(), InvalidCredentials> {
    let did = value.parse::<u64>().map_err(|_| InvalidCredentials)?;
    if did.to_string() == value {
        Ok(())
    } else {
        Err(InvalidCredentials)
    }
}
fn validate_text(value: &str) -> Result<(), InvalidCredentials> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err(InvalidCredentials)
    } else {
        Ok(())
    }
}
fn validate_pem(value: &str) -> Result<(), InvalidCredentials> {
    if value.is_empty() {
        Err(InvalidCredentials)
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/xiaomi_fixture.rs"]
pub(crate) mod fixture;
