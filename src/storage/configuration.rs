//! Private schema 10 codec and transactions for the singleton configuration.
//!
//! The stored JSON is a storage-private encoding: every layer is tagged and
//! rejects unknown fields and missing required values, and decoded values are
//! re-created through the domain constructors. Consumers only see valid
//! configuration values.

use super::{StorageError, device::DeviceStore};
use crate::{
    configuration::{CommittedConfiguration, Revocation},
    device::DeviceKey,
};
use rusqlite::params;
use std::collections::BTreeSet;

mod codec;
use codec::{decode, encode};

impl DeviceStore {
    /// Load the complete committed configuration. The singleton record is
    /// validated through the domain constructors before it is returned.
    pub(crate) fn load_configuration(&self) -> Result<CommittedConfiguration, StorageError> {
        let operation = "load bridge configuration";
        let json = self
            .store
            .connection()
            .query_row(
                "SELECT json FROM bridge_configuration WHERE id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        decode(&json).map_err(|error| self.invalid(operation, error))
    }

    /// Persist the complete next configuration and clear the revocations
    /// confirmed again in the same transaction. The value is already a valid
    /// planned configuration; only the record write and the tombstone clears
    /// happen here. A failure leaves the previous record and every tombstone
    /// untouched.
    pub(crate) fn commit_configuration(
        &self,
        configuration: &CommittedConfiguration,
        clear_revocations: &[DeviceKey],
    ) -> Result<(), StorageError> {
        self.commit_configuration_with_reset(configuration, clear_revocations, None)
    }

    /// Commit source and effective state with any protocol subscription reset.
    pub(crate) fn commit_configuration_with_reset(
        &self,
        configuration: &CommittedConfiguration,
        clear_revocations: &[DeviceKey],
        subscription_keys: Option<std::ops::Range<u16>>,
    ) -> Result<(), StorageError> {
        let operation = "commit bridge configuration";
        let json = encode(configuration).map_err(|error| self.invalid(operation, error))?;
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        if let Some(keys) = subscription_keys {
            transaction
                .execute(
                    "DELETE FROM blobs WHERE key >= ?1 AND key < ?2",
                    params![keys.start, keys.end],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        let changed = transaction
            .execute(
                "UPDATE bridge_configuration SET json=?1 WHERE id=1",
                [&json],
            )
            .map_err(|error| self.store.database_error(operation, error))?;
        if changed != 1 {
            return Err(self.invalid(operation, "Committed configuration record is missing"));
        }
        for key in clear_revocations {
            transaction
                .execute(
                    "DELETE FROM publication_revocation WHERE source=?1 AND device_id=?2",
                    params![key.source, key.id],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// Write focused revocation tombstones before the main configuration commit.
    pub(crate) fn record_revocations(
        &self,
        revocations: &[Revocation],
    ) -> Result<(), StorageError> {
        let operation = "record revocation";
        for revocation in revocations {
            validate_key(&revocation.key)
                .and_then(|()| validate_text(&revocation.reason))
                .map_err(|error| self.invalid(operation, error))?;
        }
        let transaction = self
            .store
            .connection()
            .unchecked_transaction()
            .map_err(|error| self.store.database_error(operation, error))?;
        for revocation in revocations {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO publication_revocation VALUES (?1,?2,?3)",
                    params![revocation.key.source, revocation.key.id, revocation.reason],
                )
                .map_err(|error| self.store.database_error(operation, error))?;
        }
        transaction
            .commit()
            .map_err(|error| self.store.database_error(operation, error))
    }

    /// Read every durable revocation tombstone. Corrupted or duplicated
    /// records fail the load instead of being treated as valid authority.
    pub(crate) fn load_revocations(&self) -> Result<Vec<Revocation>, StorageError> {
        let operation = "load revocations";
        let rows = self
            .store
            .connection()
            .prepare(
                "SELECT source,device_id,reason FROM publication_revocation ORDER BY source,device_id",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|error| self.store.database_error(operation, error))?;
        let mut revocations = Vec::new();
        for (source, device_id, reason) in rows {
            let key = DeviceKey::new(source, device_id);
            validate_key(&key)
                .and_then(|()| validate_text(&reason))
                .map_err(|error| self.invalid(operation, error))?;
            revocations.push(Revocation { key, reason });
        }
        let mut unique = BTreeSet::new();
        if !revocations
            .iter()
            .all(|revocation| unique.insert(revocation.key.clone()))
        {
            return Err(self.invalid(operation, "Stored revocation is duplicated"));
        }
        Ok(revocations)
    }
}

fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Stored device metadata is invalid")
    } else {
        Ok(())
    }
}

fn validate_key(key: &DeviceKey) -> Result<(), &'static str> {
    validate_text(&key.source)?;
    validate_text(&key.id)
}
