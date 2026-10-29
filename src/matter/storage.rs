use crate::storage::{MatterStore, StorageError};
use event_listener::Event;
use rs_matter::{
    error::{Error, ErrorCode},
    im::subscriptions::DEFAULT_MAX_SUBSCRIPTIONS,
    persist::{KvBlobStore, PERSISTENT_SUBSCRIPTIONS_START},
};
use std::{cell::RefCell, rc::Rc};

/// Subscription slots that must be cleared in the configuration transaction.
pub(crate) fn subscription_keys() -> std::ops::Range<u16> {
    PERSISTENT_SUBSCRIPTIONS_START
        ..PERSISTENT_SUBSCRIPTIONS_START + DEFAULT_MAX_SUBSCRIPTIONS as u16
}

/// Adapt protocol callbacks while preserving the first storage failure for the runtime.
#[derive(Clone)]
pub(super) struct StoreAdapter(Rc<Inner>);
struct Inner {
    store: MatterStore,
    failure: RefCell<Option<StorageError>>,
    failed: Event,
}
impl StoreAdapter {
    pub fn new(store: MatterStore) -> Self {
        Self(Rc::new(Inner {
            store,
            failure: RefCell::new(None),
            failed: Event::new(),
        }))
    }
    pub(super) fn record<T>(&self, result: Result<T, StorageError>) -> Result<T, Error> {
        result.map_err(|error| {
            self.0.failure.borrow_mut().get_or_insert(error);
            self.0.failed.notify(usize::MAX);
            ErrorCode::StdIoError.into()
        })
    }
    pub async fn wait_failure(&self) -> StorageError {
        loop {
            let listener = self.0.failed.listen();
            if let Some(error) = self.0.failure.borrow().clone() {
                return error;
            }
            listener.await;
        }
    }
    pub fn check_failure(&self) -> Result<(), StorageError> {
        if let Some(error) = self.0.failure.borrow().clone() {
            return Err(error);
        }
        Ok(())
    }
    pub fn with_context<T>(
        &self,
        operation: &'static str,
        result: Result<T, Error>,
    ) -> Result<T, StorageError> {
        self.check_failure()?;
        result.map_err(|source| StorageError::new(self.0.store.path(), operation, source))
    }
}
impl KvBlobStore for StoreAdapter {
    fn load<'a>(&mut self, key: u16, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
        self.check_failure().map_err(|_| ErrorCode::StdIoError)?;
        let Some(data) = self.record(self.0.store.get(key))? else {
            return Ok(None);
        };
        let target = buf.get_mut(..data.len()).ok_or(ErrorCode::NoSpace)?;
        target.copy_from_slice(&data);
        Ok(Some(target))
    }
    fn store(&mut self, key: u16, data: &[u8], _buf: &mut [u8]) -> Result<(), Error> {
        self.check_failure().map_err(|_| ErrorCode::StdIoError)?;
        self.record(self.0.store.put(key, data))
    }
    fn remove(&mut self, key: u16, _buf: &mut [u8]) -> Result<(), Error> {
        self.check_failure().map_err(|_| ErrorCode::StdIoError)?;
        self.record(self.0.store.delete(key))
    }
}

#[cfg(test)]
mod tests;
