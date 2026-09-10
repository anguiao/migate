use super::{StorageError, Store};
use rusqlite::OptionalExtension;
use std::path::Path;

/// Raw Matter records. Their encoding and key allocation belong to the protocol stack.
#[derive(Clone)]
pub struct MatterStore {
    store: Store,
}

impl MatterStore {
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn path(&self) -> &Path {
        self.store.path()
    }

    pub fn get(&self, key: u16) -> Result<Option<Vec<u8>>, StorageError> {
        self.store
            .inner
            .connection
            .query_row("SELECT value FROM blobs WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|e| {
                self.store
                    .database_error(format!("read Matter data for key {key}"), e)
            })
    }

    pub fn contains(&self, key: u16) -> Result<bool, StorageError> {
        self.store
            .inner
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE key = ?1)",
                [key],
                |row| row.get(0),
            )
            .map_err(|e| {
                self.store
                    .database_error(format!("check Matter data for key {key}"), e)
            })
    }

    pub fn put(&self, key: u16, value: &[u8]) -> Result<(), StorageError> {
        self.store
            .inner
            .connection
            .execute(
                "INSERT INTO blobs (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map(|_| ())
            .map_err(|e| {
                self.store
                    .database_error(format!("write Matter data for key {key}"), e)
            })
    }

    pub fn delete(&self, key: u16) -> Result<(), StorageError> {
        self.store
            .inner
            .connection
            .execute("DELETE FROM blobs WHERE key = ?1", [key])
            .map(|_| ())
            .map_err(|e| {
                self.store
                    .database_error(format!("delete Matter data for key {key}"), e)
            })
    }
}
