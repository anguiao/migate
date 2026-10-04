use super::{StorageError, directory::prepare_directory};
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::Path,
};

pub(super) const FILE_NAME: &str = "instance.lock";

/// Process-boundary ownership of a data directory, released by the OS on exit.
/// Keep the file in place: unlinking it would let a second process lock a new inode.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(directory: &Path) -> Result<Self, StorageError> {
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
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(StorageError::new(
                directory,
                "lock data directory",
                "Another MiGate process is using this data directory",
            )),
            Err(TryLockError::Error(error)) => {
                Err(StorageError::new(&path, "lock data directory", error))
            }
        }
    }
}
