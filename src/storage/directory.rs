use super::StorageError;
use std::{fs, path::Path};

pub(super) fn prepare_directory(directory: &Path) -> Result<(), StorageError> {
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

pub(super) fn private_permissions(path: &Path, mode: u32) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| StorageError::new(path, "set storage permissions", e))?;
    }
    Ok(())
}
