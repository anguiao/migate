use super::{StorageError, Store};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    error::Error as StdError,
    fmt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthRevision(i64);

impl AuthRevision {
    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AuthSessionGeneration(i64);

impl AuthSessionGeneration {
    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionedXiaomiRecord {
    pub record: XiaomiRecord,
    pub revision: AuthRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthSnapshot {
    pub record: Option<XiaomiRecord>,
    pub revision: AuthRevision,
    pub session_generation: AuthSessionGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiAuthObservation {
    pub uid: Option<String>,
    pub revision: AuthRevision,
    pub session_generation: AuthSessionGeneration,
}

/// A thread-safe observer for long-running Xiaomi coordinators.
#[derive(Clone, Debug)]
pub struct XiaomiAuthObserver {
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCheckFailure {
    Storage(SessionCheckError),
    InvalidCredentials,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCheckError {
    path: PathBuf,
    operation: &'static str,
    reason: String,
    kind: SessionCheckErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCheckErrorKind {
    Busy,
    Locked,
    MissingData,
    Database,
    System,
}

impl SessionCheckError {
    fn database(path: &Path, operation: &'static str, error: rusqlite::Error) -> Self {
        let (reason, kind) = match error {
            rusqlite::Error::SqliteFailure(code, _)
            | rusqlite::Error::SqlInputError { error: code, .. } => {
                let kind = match code.code {
                    rusqlite::ErrorCode::DatabaseBusy => SessionCheckErrorKind::Busy,
                    rusqlite::ErrorCode::DatabaseLocked => SessionCheckErrorKind::Locked,
                    _ => SessionCheckErrorKind::Database,
                };
                (format!("{:?}", code.code), kind)
            }
            rusqlite::Error::QueryReturnedNoRows => (
                "required row is missing".into(),
                SessionCheckErrorKind::MissingData,
            ),
            error => (error.to_string(), SessionCheckErrorKind::System),
        };
        Self {
            path: path.to_owned(),
            operation,
            reason,
            kind,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn operation(&self) -> &str {
        self.operation
    }

    pub fn kind(&self) -> &SessionCheckErrorKind {
        &self.kind
    }

    pub fn into_storage_error(self) -> StorageError {
        let path = self.path.clone();
        let operation = self.operation;
        StorageError::new(&path, operation, self)
    }
}

impl fmt::Display for SessionCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for SessionCheckError {}

impl XiaomiAuthObserver {
    pub fn observe(&self) -> Result<XiaomiAuthObservation, SessionCheckFailure> {
        let connection = self.connection()?;
        let (revision, generation, count, uid, record_revision) = connection
            .query_row(
                "SELECT auth_revision.revision, auth_session_generation.generation,
                        (SELECT count(*) FROM xiaomi_auth), xiaomi_auth.uid,
                        xiaomi_auth.revision
                 FROM auth_revision
                 JOIN auth_session_generation ON auth_session_generation.id=1
                 LEFT JOIN xiaomi_auth ON xiaomi_auth.id=1
                 WHERE auth_revision.id=1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .map_err(|error| self.storage("load fast Xiaomi authentication observation", error))?;
        if count > 1
            || (count == 1) != uid.is_some()
            || uid.as_deref().is_some_and(str::is_empty)
            || record_revision.is_some_and(|stored| stored != revision)
        {
            return Err(SessionCheckFailure::InvalidCredentials);
        }
        Ok(XiaomiAuthObservation {
            uid,
            revision: AuthRevision(revision),
            session_generation: AuthSessionGeneration(generation),
        })
    }

    pub fn snapshot(&self) -> Result<AuthSnapshot, SessionCheckFailure> {
        let connection = self.connection()?;
        let (revision, session_generation, count, record_revision, record) = connection
            .query_row(
                "SELECT auth_revision.revision, auth_session_generation.generation,
                        (SELECT count(*) FROM xiaomi_auth),
                        xiaomi_auth.uid, xiaomi_auth.region, xiaomi_auth.oauth_client_uuid,
                        xiaomi_auth.redirect_uri, xiaomi_auth.access_token,
                        xiaomi_auth.refresh_token, xiaomi_auth.expires_at,
                        xiaomi_auth.refresh_at, xiaomi_auth.virtual_did,
                        xiaomi_auth.private_key_pem, xiaomi_auth.certificate_pem,
                        xiaomi_auth.revision
                 FROM auth_revision
                 JOIN auth_session_generation ON auth_session_generation.id=1
                 LEFT JOIN xiaomi_auth ON xiaomi_auth.id=1
                 WHERE auth_revision.id=1",
                [],
                |row| {
                    let uid: Option<String> = row.get(3)?;
                    let record = match uid {
                        Some(uid) => Some(XiaomiRecord {
                            uid,
                            region: row.get(4)?,
                            oauth_client_uuid: row.get(5)?,
                            redirect_uri: row.get(6)?,
                            tokens: TokenSet {
                                access_token: row.get(7)?,
                                refresh_token: row.get(8)?,
                                expires_at: row.get(9)?,
                                refresh_at: row.get(10)?,
                            },
                            virtual_did: row.get(11)?,
                            private_key_pem: row.get(12)?,
                            certificate_pem: row.get(13)?,
                        }),
                        None => None,
                    };
                    Ok((
                        AuthRevision(row.get(0)?),
                        AuthSessionGeneration(row.get(1)?),
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(14)?,
                        record,
                    ))
                },
            )
            .map_err(|error| self.storage("load fast Xiaomi authentication snapshot", error))?;
        if count > 1
            || (count == 1) != record.is_some()
            || record_revision.is_some_and(|stored| stored != revision.get())
        {
            return Err(SessionCheckFailure::InvalidCredentials);
        }
        if let Some(record) = &record {
            validate_record(record).map_err(|_| SessionCheckFailure::InvalidCredentials)?;
        }
        Ok(AuthSnapshot {
            record,
            revision,
            session_generation,
        })
    }

    fn connection(&self) -> Result<Connection, SessionCheckFailure> {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| self.storage("open fast Xiaomi authentication observation", error))?;
        connection
            .busy_timeout(super::BUSY_TIMEOUT)
            .map_err(|error| {
                self.storage("configure fast Xiaomi authentication observation", error)
            })?;
        Ok(connection)
    }

    fn storage(&self, operation: &'static str, error: rusqlite::Error) -> SessionCheckFailure {
        SessionCheckFailure::Storage(SessionCheckError::database(&self.path, operation, error))
    }
}

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

#[derive(Clone, Eq, PartialEq)]
pub struct XiaomiRecord {
    pub uid: String,
    pub region: String,
    pub oauth_client_uuid: String,
    pub redirect_uri: String,
    pub tokens: TokenSet,
    pub virtual_did: String,
    pub private_key_pem: String,
    pub certificate_pem: String,
}

impl fmt::Debug for XiaomiRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XiaomiRecord")
            .field("uid", &self.uid)
            .field("region", &self.region)
            .field("oauth_client_uuid", &self.oauth_client_uuid)
            .field("redirect_uri", &"[REDACTED]")
            .field("tokens", &self.tokens)
            .field("virtual_did", &self.virtual_did)
            .field("private_key_pem", &"[REDACTED]")
            .field("certificate_pem", &"[REDACTED]")
            .finish()
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

    pub fn auth_observer(&self) -> XiaomiAuthObserver {
        XiaomiAuthObserver {
            path: self.path().to_owned(),
        }
    }

    pub fn load(&self) -> Result<Option<XiaomiRecord>, StorageError> {
        Ok(self.snapshot()?.record)
    }

    pub fn revision(&self) -> Result<AuthRevision, StorageError> {
        self.store
            .inner
            .connection
            .query_row("SELECT revision FROM auth_revision WHERE id=1", [], |row| {
                row.get::<_, i64>(0).map(AuthRevision)
            })
            .map_err(|error| {
                self.store
                    .database_error("load authentication revision", error)
            })
    }

    pub fn load_versioned(&self) -> Result<Option<VersionedXiaomiRecord>, StorageError> {
        let snapshot = self.snapshot()?;
        Ok(snapshot.record.map(|record| VersionedXiaomiRecord {
            record,
            revision: snapshot.revision,
        }))
    }

    pub fn snapshot(&self) -> Result<AuthSnapshot, StorageError> {
        let (revision, session_generation, count, record_revision, record) = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT auth_revision.revision, auth_session_generation.generation,
                    (SELECT count(*) FROM xiaomi_auth),
                    xiaomi_auth.uid, xiaomi_auth.region, xiaomi_auth.oauth_client_uuid,
                    xiaomi_auth.redirect_uri, xiaomi_auth.access_token, xiaomi_auth.refresh_token,
                    xiaomi_auth.expires_at, xiaomi_auth.refresh_at, xiaomi_auth.virtual_did,
                    xiaomi_auth.private_key_pem, xiaomi_auth.certificate_pem,
                    xiaomi_auth.revision
             FROM auth_revision
             JOIN auth_session_generation ON auth_session_generation.id=1
             LEFT JOIN xiaomi_auth ON xiaomi_auth.id=1
             WHERE auth_revision.id=1",
                [],
                |row| {
                    let uid: Option<String> = row.get(3)?;
                    let record = match uid {
                        Some(uid) => Some(XiaomiRecord {
                            uid,
                            region: row.get(4)?,
                            oauth_client_uuid: row.get(5)?,
                            redirect_uri: row.get(6)?,
                            tokens: TokenSet {
                                access_token: row.get(7)?,
                                refresh_token: row.get(8)?,
                                expires_at: row.get(9)?,
                                refresh_at: row.get(10)?,
                            },
                            virtual_did: row.get(11)?,
                            private_key_pem: row.get(12)?,
                            certificate_pem: row.get(13)?,
                        }),
                        None => None,
                    };
                    Ok((
                        AuthRevision(row.get(0)?),
                        AuthSessionGeneration(row.get(1)?),
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(14)?,
                        record,
                    ))
                },
            )
            .map_err(|error| self.store.database_error("load Xiaomi credentials", error))?;
        if count > 1
            || (count == 1 && record.is_none())
            || record_revision.is_some_and(|stored| stored != revision.get())
        {
            return Err(StorageError::new(
                self.path(),
                "validate Xiaomi credentials",
                InvalidCredentials,
            ));
        }
        if let Some(record) = &record {
            validate_record(record).map_err(|error| {
                StorageError::new(self.path(), "validate Xiaomi credentials", error)
            })?;
        }
        Ok(AuthSnapshot {
            record,
            revision,
            session_generation,
        })
    }

    pub fn replace(&self, record: &XiaomiRecord) -> Result<(), StorageError> {
        validate_record(record).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi credentials", error)
        })?;
        self.transaction("replace Xiaomi credentials", |transaction| {
            let revision =
                bump_revision(transaction, None)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            bump_session_generation(transaction)?;
            deactivate_other_accounts(transaction, &record.uid)?;
            transaction.execute(
                "INSERT INTO xiaomi_auth (
                    id, uid, region, oauth_client_uuid, redirect_uri,
                    access_token, refresh_token, expires_at, refresh_at,
                    virtual_did, private_key_pem, certificate_pem, revision
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (id) DO UPDATE SET
                    uid = excluded.uid,
                    region = excluded.region,
                    oauth_client_uuid = excluded.oauth_client_uuid,
                    redirect_uri = excluded.redirect_uri,
                    access_token = excluded.access_token,
                    refresh_token = excluded.refresh_token,
                    expires_at = excluded.expires_at,
                    refresh_at = excluded.refresh_at,
                    virtual_did = excluded.virtual_did,
                    private_key_pem = excluded.private_key_pem,
                    certificate_pem = excluded.certificate_pem,
                    revision = excluded.revision",
                (
                    &record.uid,
                    &record.region,
                    &record.oauth_client_uuid,
                    &record.redirect_uri,
                    &record.tokens.access_token,
                    &record.tokens.refresh_token,
                    record.tokens.expires_at,
                    record.tokens.refresh_at,
                    &record.virtual_did,
                    &record.private_key_pem,
                    &record.certificate_pem,
                    revision,
                ),
            )?;
            Ok(())
        })
    }

    pub fn replace_if_revision(
        &self,
        expected: AuthRevision,
        record: &XiaomiRecord,
    ) -> Result<Option<AuthRevision>, StorageError> {
        validate_record(record).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi credentials", error)
        })?;
        self.transaction_value("replace Xiaomi credentials", |transaction| {
            let Some(revision) = bump_revision(transaction, Some(expected))? else {
                return Ok(None);
            };
            bump_session_generation(transaction)?;
            deactivate_other_accounts(transaction, &record.uid)?;
            transaction.execute(
                "INSERT INTO xiaomi_auth (
                    id,uid,region,oauth_client_uuid,redirect_uri,access_token,refresh_token,
                    expires_at,refresh_at,virtual_did,private_key_pem,certificate_pem,revision
                 )
                 VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                    uid=excluded.uid,region=excluded.region,
                    oauth_client_uuid=excluded.oauth_client_uuid,
                    redirect_uri=excluded.redirect_uri,access_token=excluded.access_token,
                    refresh_token=excluded.refresh_token,expires_at=excluded.expires_at,
                    refresh_at=excluded.refresh_at,virtual_did=excluded.virtual_did,
                    private_key_pem=excluded.private_key_pem,
                    certificate_pem=excluded.certificate_pem,revision=excluded.revision",
                params![
                    record.uid,
                    record.region,
                    record.oauth_client_uuid,
                    record.redirect_uri,
                    record.tokens.access_token,
                    record.tokens.refresh_token,
                    record.tokens.expires_at,
                    record.tokens.refresh_at,
                    record.virtual_did,
                    record.private_key_pem,
                    record.certificate_pem,
                    revision
                ],
            )?;
            Ok(Some(AuthRevision(revision)))
        })
    }

    pub fn update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.transaction("update Xiaomi tokens", |transaction| {
            let revision =
                bump_revision(transaction, None)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET access_token = ?1, refresh_token = ?2,
                    expires_at = ?3, refresh_at = ?4, revision=?5 WHERE id = 1",
                (
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                    revision,
                ),
            )?;
            require_record(changed)
        })
    }

    pub fn update_tokens_if_revision(
        &self,
        expected: AuthRevision,
        tokens: &TokenSet,
    ) -> Result<Option<AuthRevision>, StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.transaction_value("update Xiaomi tokens", |transaction| {
            let Some(revision) = bump_revision(transaction, Some(expected))? else {
                return Ok(None);
            };
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET access_token=?1,refresh_token=?2,
                    expires_at=?3,refresh_at=?4,revision=?5 WHERE id=1",
                params![
                    tokens.access_token,
                    tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                    revision
                ],
            )?;
            require_record(changed)?;
            Ok(Some(AuthRevision(revision)))
        })
    }

    pub fn update_certificate(&self, certificate_pem: &str) -> Result<(), StorageError> {
        validate_pem(certificate_pem).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi certificate", error)
        })?;
        self.transaction("update Xiaomi certificate", |transaction| {
            let revision =
                bump_revision(transaction, None)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET certificate_pem = ?1, revision=?2 WHERE id = 1",
                params![certificate_pem, revision],
            )?;
            require_record(changed)
        })
    }

    pub fn update_certificate_if_revision(
        &self,
        expected: AuthRevision,
        certificate_pem: &str,
    ) -> Result<Option<AuthRevision>, StorageError> {
        validate_pem(certificate_pem).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi certificate", error)
        })?;
        self.transaction_value("update Xiaomi certificate", |transaction| {
            let Some(revision) = bump_revision(transaction, Some(expected))? else {
                return Ok(None);
            };
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET certificate_pem=?1,revision=?2 WHERE id=1",
                params![certificate_pem, revision],
            )?;
            require_record(changed)?;
            Ok(Some(AuthRevision(revision)))
        })
    }

    pub fn logout(&self) -> Result<(), StorageError> {
        self.transaction("delete Xiaomi credentials", |transaction| {
            bump_revision(transaction, None)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            bump_session_generation(transaction)?;
            transaction.execute("DELETE FROM xiaomi_auth", [])?;
            transaction.execute("DELETE FROM device_tokens", [])?;
            transaction.execute("UPDATE devices SET admitted=0", [])?;
            Ok(())
        })
    }

    fn transaction(
        &self,
        operation: &'static str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Result<(), StorageError> {
        self.transaction_value(operation, body)
    }

    fn transaction_value<T>(
        &self,
        operation: &'static str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, StorageError> {
        let transaction = self
            .store
            .inner
            .connection
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        let value =
            body(&transaction).map_err(|error| self.store.database_error(operation, error))?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))?;
        Ok(value)
    }
}

fn bump_session_generation(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    let changed = transaction.execute(
        "UPDATE auth_session_generation SET generation=generation+1 WHERE id=1",
        [],
    )?;
    require_record(changed)
}

fn bump_revision(
    transaction: &rusqlite::Transaction<'_>,
    expected: Option<AuthRevision>,
) -> rusqlite::Result<Option<i64>> {
    let changed = match expected {
        Some(expected) => transaction.execute(
            "UPDATE auth_revision SET revision=revision+1 WHERE id=1 AND revision=?1",
            [expected.get()],
        )?,
        None => transaction.execute(
            "UPDATE auth_revision SET revision=revision+1 WHERE id=1",
            [],
        )?,
    };
    if changed == 0 {
        return Ok(None);
    }
    transaction
        .query_row("SELECT revision FROM auth_revision WHERE id=1", [], |row| {
            row.get(0)
        })
        .map(Some)
}

fn deactivate_other_accounts(
    transaction: &rusqlite::Transaction<'_>,
    uid: &str,
) -> rusqlite::Result<()> {
    transaction.execute("DELETE FROM device_tokens WHERE account_uid<>?1", [uid])?;
    transaction.execute("UPDATE devices SET admitted=0 WHERE account_uid<>?1", [uid])?;
    transaction.execute(
        "UPDATE feature_identities SET active=0 WHERE account_uid<>?1",
        [uid],
    )?;
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

fn validate_record(record: &XiaomiRecord) -> Result<(), InvalidCredentials> {
    validate_text(&record.uid)?;
    if record.region != "cn" {
        return Err(InvalidCredentials);
    }
    validate_uuid(&record.oauth_client_uuid)?;
    validate_text(&record.redirect_uri)?;
    validate_tokens(&record.tokens)?;
    validate_virtual_did(&record.virtual_did)?;
    validate_pem(&record.private_key_pem)?;
    validate_pem(&record.certificate_pem)
}

fn validate_tokens(tokens: &TokenSet) -> Result<(), InvalidCredentials> {
    validate_text(&tokens.access_token)?;
    validate_text(&tokens.refresh_token)?;
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
