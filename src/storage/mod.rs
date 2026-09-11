mod error;
mod identity;
mod matter;
mod migration;
mod xiaomi;

pub use error::StorageError;
pub use identity::Identity;
pub use matter::MatterStore;
pub use xiaomi::{TokenSet, XiaomiRecord, XiaomiStore};

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
        prepare_directory(directory)?;
        let path = directory.join("state.db");
        let new = !path
            .try_exists()
            .map_err(|e| StorageError::new(&path, "locate database", e))?;
        if new {
            require_empty_directory(directory)?;
        }

        let mut connection = Connection::open(&path)
            .map_err(|e| StorageError::database(&path, "open database", e))?;
        private_permissions(&path, 0o600)?;
        connection
            .execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = EXTRA;")
            .map_err(|e| StorageError::database(&path, "configure database", e))?;
        if new {
            initialize(&mut connection)
                .map_err(|e| StorageError::database(&path, "initialize database", e))?;
        } else {
            migration::migrate(&mut connection).map_err(|error| match error {
                migration::MigrationError::Database(error) => {
                    StorageError::database(&path, "migrate database", error)
                }
                error => StorageError::new(&path, "validate database schema", error),
            })?;
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

    fn database_error(&self, operation: impl Into<String>, error: rusqlite::Error) -> StorageError {
        StorageError::database(self.path(), operation, error)
    }
}

fn prepare_directory(directory: &Path) -> Result<(), StorageError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .map_err(|e| StorageError::new(directory, "create data directory", e))?;
    private_permissions(directory, 0o700)
}

fn require_empty_directory(directory: &Path) -> Result<(), StorageError> {
    let entry = fs::read_dir(directory)
        .and_then(|mut entries| entries.next().transpose())
        .map_err(|e| StorageError::new(directory, "read data directory", e))?;
    if entry.is_some() {
        return Err(StorageError::new(
            directory,
            "initialize data directory",
            "Data directory initialization is incomplete",
        ));
    }
    Ok(())
}

fn initialize(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(include_str!("schema.sql"))?;
    identity::insert(&transaction)?;
    transaction.commit()
}

fn private_permissions(path: &Path, mode: u32) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| StorageError::new(path, "set storage permissions", e))?;
    }
    Ok(())
}
