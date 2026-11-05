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

    /// Replace the complete record transactionally. Independent maintenance
    /// should use the focused update methods so other groups are preserved.
    pub fn replace(&self, record: &XiaomiRecord) -> Result<(), StorageError> {
        self.validate(record)?;
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            tx.execute("DELETE FROM xiaomi_lan WHERE uid <> ?1", [&record.uid])?;
            tx.execute(
                "INSERT INTO xiaomi_account (id, uid, region, mijia_generation) VALUES (1, ?1, ?2, ?3)
                ON CONFLICT (id) DO UPDATE SET uid = excluded.uid, region = excluded.region, mijia_generation = excluded.mijia_generation",
                (&record.uid, &record.region, Uuid::new_v4().to_string()),
            )?;
            for table in [
                "xiaomi_account_recovery",
                "xiaomi_mijia_session",
                "xiaomi_ha_oauth",
                "xiaomi_ha_client",
                "xiaomi_hub",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
            if let Some(recovery) = &record.recovery {
                write_recovery(&tx, &recovery.credentials, recovery.stop)?;
            }
            if let Some(session) = &record.mijia {
                write_session(&tx, session)?;
            }
            if let Some(ha) = &record.ha {
                write_ha(&tx, ha)?;
            }
            if let Some(hub) = &record.hub {
                write_hub(&tx, hub)?;
            }
            tx.commit()
        })()
        .map_err(|error| {
            self.store
                .database_error("replace Xiaomi credentials", error)
        })
    }

    /// Commit all credentials obtained during one login while preserving the
    /// existing hub identity. This is the only maintenance operation that
    /// clears both durable stop flags.
    pub fn commit_login(
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
            let current: Option<String> = tx.query_row("SELECT uid FROM xiaomi_account WHERE id = 1", [], |row| row.get(0)).optional()?;
            if current.as_deref().is_some_and(|uid| uid != candidate.uid) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let old_client: Option<String> = tx.query_row("SELECT client_uuid FROM xiaomi_ha_client WHERE id = 1", [], |row| row.get(0)).optional()?;
            if old_client.as_deref().is_some_and(|uuid| uuid != client_uuid) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            tx.execute("INSERT INTO xiaomi_account (id, uid, region, mijia_generation) VALUES (1, ?1, 'cn', ?2) ON CONFLICT(id) DO UPDATE SET mijia_generation = excluded.mijia_generation", (&candidate.uid, Uuid::new_v4().to_string()))?;
            write_recovery(&tx, credentials, None)?;
            write_session(&tx, session)?;
            write_ha(&tx, candidate.ha.as_ref().expect("candidate includes HA client"))?;
            tx.commit()
        })().map_err(|error| self.store.database_error("commit Xiaomi login", error))
    }

    /// Replace a restored account credential and device session together.
    pub fn update_recovery(
        &self,
        credentials: &AccountCredentials,
        session: &MijiaSession,
    ) -> Result<(), StorageError> {
        let current = self.load()?.ok_or_else(|| {
            StorageError::new(self.path(), "update Xiaomi recovery", InvalidCredentials)
        })?;
        if current.uid != credentials.user_id() || current.uid != session.user_id() {
            return Err(StorageError::new(
                self.path(),
                "validate Xiaomi recovery",
                InvalidCredentials,
            ));
        }
        let stop = current.recovery.as_ref().and_then(|recovery| recovery.stop);
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            tx.execute(
                "UPDATE xiaomi_account SET mijia_generation = ?1 WHERE id = 1",
                [Uuid::new_v4().to_string()],
            )
            .and_then(require_record)?;
            write_recovery(&tx, credentials, stop)?;
            write_session(&tx, session)?;
            tx.commit()
        })()
        .map_err(|error| self.store.database_error("update Xiaomi recovery", error))
    }

    /// Capture the generation alongside an account snapshot before starting
    /// asynchronous device requests or account recovery.
    pub(crate) fn mijia_generation(&self, uid: &str) -> Result<Option<String>, StorageError> {
        self.store
            .connection()
            .query_row(
                "SELECT mijia_generation FROM xiaomi_account WHERE id = 1 AND uid = ?1",
                [uid],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                self.store
                    .database_error("read Mijia session generation", error)
            })
    }

    pub(crate) fn update_recovery_if_current(
        &self,
        credentials: &AccountCredentials,
        session: &MijiaSession,
        generation: &str,
    ) -> Result<bool, StorageError> {
        if credentials.user_id() != session.user_id() {
            return Err(StorageError::new(
                self.path(),
                "validate Xiaomi recovery",
                InvalidCredentials,
            ));
        }
        (|| {
            let tx = self.store.connection().unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE xiaomi_account SET mijia_generation = ?1
                 WHERE id = 1 AND uid = ?2 AND mijia_generation = ?3
                   AND EXISTS (SELECT 1 FROM xiaomi_account_recovery WHERE id = 1 AND stopped IS NULL)",
                (Uuid::new_v4().to_string(), credentials.user_id(), generation),
            )?;
            if changed == 0 {
                return Ok(false);
            }
            write_recovery(&tx, credentials, None)?;
            write_session(&tx, session)?;
            tx.commit()?;
            Ok(true)
        })()
        .map_err(|error| self.store.database_error("update Xiaomi recovery", error))
    }

    pub(crate) fn stop_recovery_if_current(
        &self,
        uid: &str,
        generation: &str,
        stop: RecoveryStop,
    ) -> Result<bool, StorageError> {
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_account_recovery SET stopped = ?1
                 WHERE id = 1 AND stopped IS NULL
                   AND EXISTS (SELECT 1 FROM xiaomi_account
                               WHERE id = 1 AND uid = ?2 AND mijia_generation = ?3)",
                (stop.as_str(), uid, generation),
            )
            .map(|changed| changed == 1)
            .map_err(|error| self.store.database_error("stop Xiaomi recovery", error))
    }

    pub fn stop_recovery(&self, stop: RecoveryStop) -> Result<(), StorageError> {
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_account_recovery SET stopped = ?1 WHERE id = 1",
                [stop.as_str()],
            )
            .and_then(require_record)
            .map_err(|error| self.store.database_error("stop Xiaomi recovery", error))
    }

    pub fn stop_ha_refresh(&self) -> Result<(), StorageError> {
        self.store
            .connection()
            .execute("UPDATE xiaomi_ha_client SET stopped = 1 WHERE id = 1", [])
            .and_then(require_record)
            .map_err(|error| self.store.database_error("stop HA refresh", error))
    }

    pub(crate) fn ha_refresh_generation(
        &self,
        redirect_uri: &str,
        tokens: &TokenSet,
    ) -> Result<Option<String>, StorageError> {
        self.store
            .connection()
            .query_row(
                "SELECT oauth.generation FROM xiaomi_ha_oauth AS oauth
             JOIN xiaomi_ha_client AS client ON client.id = oauth.id
             WHERE oauth.id = 1 AND client.stopped = 0
               AND oauth.redirect_uri = ?1 AND oauth.access_token = ?2
               AND oauth.refresh_token = ?3 AND oauth.expires_at = ?4 AND oauth.refresh_at = ?5",
                (
                    redirect_uri,
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                ),
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                self.store
                    .database_error("read HA authorization generation", error)
            })
    }

    pub(crate) fn stop_ha_refresh_if_current(
        &self,
        generation: &str,
    ) -> Result<bool, StorageError> {
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_ha_client SET stopped = 1 WHERE id = 1
             AND EXISTS (SELECT 1 FROM xiaomi_ha_oauth WHERE id = 1 AND generation = ?1)",
                [generation],
            )
            .map(|changed| changed == 1)
            .map_err(|error| self.store.database_error("stop HA refresh", error))
    }

    pub fn update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.store.connection().execute("UPDATE xiaomi_ha_oauth SET access_token = ?1, refresh_token = ?2, expires_at = ?3, refresh_at = ?4, generation = ?5 WHERE id = 1",
            (&tokens.access_token, &tokens.refresh_token, tokens.expires_at, tokens.refresh_at, Uuid::new_v4().to_string()))
            .and_then(require_record).map_err(|error| self.store.database_error("update Xiaomi tokens", error))
    }

    pub(crate) fn update_tokens_if_current(
        &self,
        tokens: &TokenSet,
        generation: &str,
    ) -> Result<bool, StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_ha_oauth SET access_token = ?1, refresh_token = ?2,
                expires_at = ?3, refresh_at = ?4, generation = ?5
             WHERE id = 1 AND generation = ?6
               AND EXISTS (SELECT 1 FROM xiaomi_ha_client WHERE id = 1 AND stopped = 0)",
                (
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                    Uuid::new_v4().to_string(),
                    generation,
                ),
            )
            .map(|changed| changed == 1)
            .map_err(|error| self.store.database_error("update Xiaomi tokens", error))
    }

    pub fn update_hub(&self, hub: &HubIdentity) -> Result<(), StorageError> {
        validate_hub(hub).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi hub identity", error)
        })?;
        if self.load()?.is_none() {
            return Err(StorageError::new(
                self.path(),
                "update Xiaomi hub identity",
                InvalidCredentials,
            ));
        }
        write_hub(self.store.connection(), hub).map_err(|error| {
            self.store
                .database_error("update Xiaomi hub identity", error)
        })
    }

    pub fn update_certificate(&self, certificate_pem: &str) -> Result<(), StorageError> {
        validate_pem(certificate_pem).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi certificate", error)
        })?;
        self.store
            .connection()
            .execute(
                "UPDATE xiaomi_hub SET certificate_pem = ?1 WHERE id = 1",
                [certificate_pem],
            )
            .and_then(require_record)
            .map_err(|error| {
                self.store
                    .database_error("update Xiaomi certificate", error)
            })
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
        })
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
        db.execute("INSERT INTO xiaomi_ha_oauth (id, redirect_uri, access_token, refresh_token, expires_at, refresh_at, generation) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET redirect_uri=excluded.redirect_uri, access_token=excluded.access_token, refresh_token=excluded.refresh_token, expires_at=excluded.expires_at, refresh_at=excluded.refresh_at, generation=excluded.generation",
            (&authorization.redirect_uri, &tokens.access_token, &tokens.refresh_token, tokens.expires_at, tokens.refresh_at, Uuid::new_v4().to_string()))?;
    }
    Ok(())
}

fn write_hub(db: &Connection, hub: &HubIdentity) -> rusqlite::Result<()> {
    db.execute("INSERT INTO xiaomi_hub (id, virtual_did, private_key_pem, certificate_pem) VALUES (1, ?1, ?2, ?3)
        ON CONFLICT(id) DO UPDATE SET virtual_did=excluded.virtual_did, private_key_pem=excluded.private_key_pem, certificate_pem=excluded.certificate_pem",
        (&hub.virtual_did, &hub.private_key_pem, &hub.certificate_pem))?;
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
fn require_record(changed: usize) -> rusqlite::Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}
