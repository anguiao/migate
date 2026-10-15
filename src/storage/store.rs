use super::directory::{prepare_directory, private_permissions};
use super::{
    StorageError,
    device::DeviceStore,
    identity::{self, Identity},
    lock,
    matter::MatterStore,
    schema,
    xiaomi::XiaomiStore,
};
use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

/// Application storage. Clones share one connection on the local executor.
#[derive(Clone)]
pub struct Store {
    inner: Rc<Inner>,
}

struct Inner {
    path: PathBuf,
    connection: Connection,
}

impl Store {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        let directory = directory.as_ref();
        // Do not prepare (and chmod) an existing data directory before the
        // database itself is known to be a supported format.
        let directory_ready = fs::metadata(directory)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if !directory_ready {
            prepare_directory(directory)?;
        }
        let path = directory.join("state.db");
        let new = !path
            .try_exists()
            .map_err(|e| StorageError::new(&path, "locate database", e))?;
        if new {
            require_empty_directory(directory)?;
        }

        let mut connection = Connection::open(&path)
            .map_err(|e| StorageError::database(&path, "open database", e))?;
        if !new {
            // Reject unsupported or damaged databases before changing
            // permissions, the journal mode or any other file state.
            schema::validate(&connection)
                .map_err(|error| StorageError::new(&path, "validate database schema", error))?;
        }
        prepare_directory(directory)?;
        private_permissions(&path, 0o600)?;
        connection
            .execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = EXTRA;")
            .map_err(|e| StorageError::database(&path, "configure database", e))?;
        if new {
            initialize(&mut connection)
                .map_err(|e| StorageError::database(&path, "initialize database", e))?;
        }
        Ok(Self {
            inner: Rc::new(Inner { path, connection }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn load_identity(&self) -> Result<Identity, StorageError> {
        identity::load(&self.inner.connection).map_err(|e| self.database_error("load identity", e))
    }

    pub fn matter(&self) -> MatterStore {
        MatterStore::new(self.clone())
    }

    pub fn xiaomi(&self) -> XiaomiStore {
        XiaomiStore::new(self.clone())
    }

    pub fn devices(&self) -> DeviceStore {
        DeviceStore::new(self.clone())
    }

    pub(super) fn connection(&self) -> &Connection {
        &self.inner.connection
    }

    pub(super) fn database_error(
        &self,
        operation: impl Into<String>,
        error: rusqlite::Error,
    ) -> StorageError {
        StorageError::database(self.path(), operation, error)
    }
}

fn require_empty_directory(directory: &Path) -> Result<(), StorageError> {
    let entries = fs::read_dir(directory)
        .map_err(|e| StorageError::new(directory, "read data directory", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| StorageError::new(directory, "read data directory", e))?;
        if entry.file_name() != lock::FILE_NAME {
            return Err(StorageError::new(
                directory,
                "initialize data directory",
                "Data directory initialization is incomplete",
            ));
        }
    }
    Ok(())
}

fn initialize(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(schema::SCHEMA)?;
    identity::insert(&transaction)?;
    transaction.commit()
}
