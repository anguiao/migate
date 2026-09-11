use super::{StorageError, Store};
use rusqlite::OptionalExtension;
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

    pub fn load(&self) -> Result<Option<XiaomiRecord>, StorageError> {
        let invalid_slot = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM xiaomi_auth WHERE id <> 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| self.store.database_error("load Xiaomi credentials", error))?;
        if invalid_slot {
            return Err(StorageError::new(
                self.path(),
                "validate Xiaomi credentials",
                InvalidCredentials,
            ));
        }
        let record = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT uid, region, oauth_client_uuid, redirect_uri,
                        access_token, refresh_token, expires_at, refresh_at,
                        virtual_did, private_key_pem, certificate_pem
                 FROM xiaomi_auth WHERE id = 1",
                [],
                |row| {
                    Ok(XiaomiRecord {
                        uid: row.get(0)?,
                        region: row.get(1)?,
                        oauth_client_uuid: row.get(2)?,
                        redirect_uri: row.get(3)?,
                        tokens: TokenSet {
                            access_token: row.get(4)?,
                            refresh_token: row.get(5)?,
                            expires_at: row.get(6)?,
                            refresh_at: row.get(7)?,
                        },
                        virtual_did: row.get(8)?,
                        private_key_pem: row.get(9)?,
                        certificate_pem: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(|error| self.store.database_error("load Xiaomi credentials", error))?;
        if let Some(record) = &record {
            validate_record(record).map_err(|error| {
                StorageError::new(self.path(), "validate Xiaomi credentials", error)
            })?;
        }
        Ok(record)
    }

    pub fn replace(&self, record: &XiaomiRecord) -> Result<(), StorageError> {
        validate_record(record).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi credentials", error)
        })?;
        self.transaction("replace Xiaomi credentials", |transaction| {
            transaction.execute(
                "INSERT INTO xiaomi_auth (
                    id, uid, region, oauth_client_uuid, redirect_uri,
                    access_token, refresh_token, expires_at, refresh_at,
                    virtual_did, private_key_pem, certificate_pem
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
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
                    certificate_pem = excluded.certificate_pem",
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
                ),
            )?;
            Ok(())
        })
    }

    pub fn update_tokens(&self, tokens: &TokenSet) -> Result<(), StorageError> {
        validate_tokens(tokens)
            .map_err(|error| StorageError::new(self.path(), "validate Xiaomi tokens", error))?;
        self.transaction("update Xiaomi tokens", |transaction| {
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET access_token = ?1, refresh_token = ?2,
                    expires_at = ?3, refresh_at = ?4 WHERE id = 1",
                (
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                    tokens.refresh_at,
                ),
            )?;
            require_record(changed)
        })
    }

    pub fn update_certificate(&self, certificate_pem: &str) -> Result<(), StorageError> {
        validate_pem(certificate_pem).map_err(|error| {
            StorageError::new(self.path(), "validate Xiaomi certificate", error)
        })?;
        self.transaction("update Xiaomi certificate", |transaction| {
            let changed = transaction.execute(
                "UPDATE xiaomi_auth SET certificate_pem = ?1 WHERE id = 1",
                [certificate_pem],
            )?;
            require_record(changed)
        })
    }

    pub fn logout(&self) -> Result<(), StorageError> {
        self.transaction("delete Xiaomi credentials", |transaction| {
            transaction.execute("DELETE FROM xiaomi_auth", [])?;
            Ok(())
        })
    }

    fn transaction(
        &self,
        operation: &'static str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Result<(), StorageError> {
        let transaction = self
            .store
            .inner
            .connection
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        body(&transaction).map_err(|error| self.store.database_error(operation, error))?;
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }
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
