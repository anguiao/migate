use super::{StorageError, Store};

/// Application storage for the committed bridge configuration and its
/// revocations. The complete configuration value and its private codec live in
/// `storage::configuration`; native facts and the derived group graph belong to
/// the `configuration` model.
#[derive(Clone)]
pub struct DeviceStore {
    pub(super) store: Store,
}

impl DeviceStore {
    pub(super) fn new(store: Store) -> Self {
        Self { store }
    }

    pub(crate) fn invalid(&self, operation: &'static str, error: &'static str) -> StorageError {
        StorageError::new(self.store.path(), operation, error)
    }
}
