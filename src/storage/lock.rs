use super::{StorageError, directory::prepare_directory};
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
};

pub(super) const FILE_NAME: &str = "instance.lock";
pub(crate) const SOCKET_FILE_NAME: &str = "migate.sock";

/// Process-boundary ownership of a data directory, released by the OS on exit.
/// Keep the file in place: unlinking it would let a second process lock a new inode.
pub struct InstanceLock {
    _file: File,
    directory: PathBuf,
}

impl InstanceLock {
    pub fn acquire(directory: &Path) -> Result<Self, StorageError> {
        Self::try_acquire(directory)?.ok_or_else(|| {
            StorageError::new(
                directory,
                "lock data directory",
                "Another MiGate process is using this data directory",
            )
        })
    }

    pub fn try_acquire(directory: &Path) -> Result<Option<Self>, StorageError> {
        prepare_directory(directory)?;
        let path = directory.join(FILE_NAME);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|error| StorageError::new(&path, "open instance lock", error))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                directory: directory.to_owned(),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(StorageError::new(&path, "lock data directory", error))
            }
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}
