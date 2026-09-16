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

    pub fn topology_signature(&self) -> Result<String, StorageError> {
        let signature: String = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT signature FROM matter_topology WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                self.store
                    .database_error("read Matter topology signature", e)
            })?;
        if !signature.is_empty()
            && (signature.len() != 40 || !signature.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(StorageError::new(
                self.store.path(),
                "read Matter topology signature",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid topology signature",
                ),
            ));
        }
        Ok(signature)
    }

    pub fn feature_label(&self, endpoint: u16) -> Result<Option<String>, StorageError> {
        let label: Option<String> = self
            .store
            .inner
            .connection
            .query_row(
                "SELECT label FROM matter_feature_labels WHERE endpoint = ?1",
                [endpoint],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| self.store.database_error("read Matter feature label", e))?;
        if label.as_ref().is_some_and(|value| value.len() > 32) {
            return Err(StorageError::new(
                self.store.path(),
                "read Matter feature label",
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid feature label"),
            ));
        }
        Ok(label)
    }

    pub fn save_feature_label(&self, endpoint: u16, label: &str) -> Result<(), StorageError> {
        self.store
            .inner
            .connection
            .execute(
                "INSERT INTO matter_feature_labels (endpoint, label) VALUES (?1, ?2)
             ON CONFLICT (endpoint) DO UPDATE SET label = excluded.label",
                (endpoint, label),
            )
            .map(|_| ())
            .map_err(|e| self.store.database_error("write Matter feature label", e))
    }

    pub fn save_topology(
        &self,
        key: u16,
        value: &[u8],
        signature: &str,
    ) -> Result<(), StorageError> {
        let transaction = self
            .store
            .inner
            .connection
            .unchecked_transaction()
            .map_err(|e| {
                self.store
                    .database_error("begin Matter topology transaction", e)
            })?;
        transaction
            .execute(
                "INSERT INTO blobs (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map_err(|e| {
                self.store
                    .database_error(format!("write Matter data for key {key}"), e)
            })?;
        let changed = transaction
            .execute(
                "UPDATE matter_topology SET signature = ?1 WHERE id = 1",
                [signature],
            )
            .map_err(|e| {
                self.store
                    .database_error("write Matter topology signature", e)
            })?;
        if changed != 1 {
            return Err(StorageError::new(
                self.store.path(),
                "write Matter topology signature",
                rusqlite::Error::QueryReturnedNoRows,
            ));
        }
        transaction.commit().map_err(|e| {
            self.store
                .database_error("commit Matter topology transaction", e)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_blob_and_signature_commit_or_roll_back_together() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let matter = store.matter();
        matter.put(1, b"old").unwrap();
        let observer = rusqlite::Connection::open(store.path()).unwrap();
        observer
            .execute_batch(
                "CREATE TRIGGER reject_topology BEFORE UPDATE ON matter_topology
             BEGIN SELECT RAISE(FAIL, 'rejected'); END;",
            )
            .unwrap();

        assert!(matter.save_topology(1, b"new", &"1".repeat(40)).is_err());
        assert_eq!(matter.get(1).unwrap().as_deref(), Some(b"old".as_slice()));
        assert_eq!(matter.topology_signature().unwrap(), "");
    }

    #[test]
    fn missing_topology_singleton_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let observer = rusqlite::Connection::open(store.path()).unwrap();
        observer.execute("DELETE FROM matter_topology", []).unwrap();

        assert!(store.matter().topology_signature().is_err());
        assert!(
            store
                .matter()
                .save_topology(1, b"blob", &"1".repeat(40))
                .is_err()
        );
        assert_eq!(store.matter().get(1).unwrap(), None);
    }

    #[test]
    fn corrupt_topology_metadata_is_rejected_on_read() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        db.execute("UPDATE matter_topology SET signature = 'not-a-digest'", [])
            .unwrap();
        db.execute(
            "INSERT INTO feature_identities (
                account_uid,home_id,parent_did,service_instance,role,endpoint,public_id,active
             ) VALUES ('u','h','d',2,'temperature_sensor',2,?1,1)",
            ["1".repeat(32)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO matter_feature_labels (endpoint,label) VALUES (2,?1)",
            ["x".repeat(33)],
        )
        .unwrap();

        assert!(store.matter().topology_signature().is_err());
        assert!(store.matter().feature_label(2).is_err());
    }
}
