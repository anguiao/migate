use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
};

/// A failure while accessing or restoring stored data.
#[derive(Clone, Debug)]
pub struct StorageError(Rc<Details>);

#[derive(Debug)]
struct Details {
    path: PathBuf,
    operation: String,
    source: Box<dyn Error>,
}

impl StorageError {
    pub(crate) fn new(
        path: &Path,
        operation: impl Into<String>,
        source: impl Into<Box<dyn Error>>,
    ) -> Self {
        Self(Rc::new(Details {
            path: path.into(),
            operation: operation.into(),
            source: source.into(),
        }))
    }

    pub(super) fn database(
        path: &Path,
        operation: impl Into<String>,
        source: rusqlite::Error,
    ) -> Self {
        // SQLite messages may quote stored values. Preserve the error code without that text.
        let source = match source {
            rusqlite::Error::SqliteFailure(code, _)
            | rusqlite::Error::SqlInputError { error: code, .. } => {
                rusqlite::Error::SqliteFailure(code, None)
            }
            source => source,
        };
        Self::new(path, operation, source)
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    pub fn operation(&self) -> &str {
        &self.0.operation
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to {} ({}): {}",
            self.operation(),
            self.path().display(),
            self.0.source
        )
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.source.as_ref())
    }
}
