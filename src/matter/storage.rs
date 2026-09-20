use crate::storage::{MatterStore, StorageError};
use event_listener::Event;
use rs_matter::{
    error::{Error, ErrorCode},
    persist::KvBlobStore,
};
use std::{cell::RefCell, rc::Rc};

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
    pub fn storage(&self) -> &MatterStore {
        &self.0.store
    }
    pub(super) fn record<T>(&self, result: Result<T, StorageError>) -> Result<T, Error> {
        result.map_err(|error| {
            self.0.failure.borrow_mut().get_or_insert(error);
            self.0.failed.notify(usize::MAX);
            ErrorCode::StdIoError.into()
        })
    }
    pub(super) fn capture<T>(&self, result: Result<T, StorageError>) -> Result<T, StorageError> {
        result.inspect_err(|error| {
            self.0
                .failure
                .borrow_mut()
                .get_or_insert_with(|| error.clone());
            self.0.failed.notify(usize::MAX);
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

#[derive(Clone)]
pub(super) struct EndpointSceneStore {
    store: StoreAdapter,
    endpoint: u16,
}

impl EndpointSceneStore {
    pub(super) fn new(store: StoreAdapter, endpoint: u16) -> Self {
        Self { store, endpoint }
    }
}

impl KvBlobStore for EndpointSceneStore {
    fn load<'a>(&mut self, key: u16, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
        if key != rs_matter::persist::SCENES_KEY {
            return self.store.load(key, buf);
        }
        self.store
            .check_failure()
            .map_err(|_| ErrorCode::StdIoError)?;
        let Some(data) = self
            .store
            .record(self.store.storage().endpoint_scenes(self.endpoint))?
        else {
            return Ok(None);
        };
        let target = buf.get_mut(..data.len()).ok_or(ErrorCode::NoSpace)?;
        target.copy_from_slice(&data);
        Ok(Some(target))
    }

    fn store(&mut self, key: u16, data: &[u8], buf: &mut [u8]) -> Result<(), Error> {
        if key != rs_matter::persist::SCENES_KEY {
            return self.store.store(key, data, buf);
        }
        self.store.record(
            self.store
                .storage()
                .save_endpoint_scenes(self.endpoint, data),
        )
    }

    fn remove(&mut self, key: u16, buf: &mut [u8]) -> Result<(), Error> {
        if key != rs_matter::persist::SCENES_KEY {
            return self.store.remove(key, buf);
        }
        self.store
            .record(self.store.storage().delete_endpoint_scenes(self.endpoint))
    }
}

pub(super) struct TopologyStore {
    store: StoreAdapter,
    signature: String,
}

impl TopologyStore {
    pub(super) fn new(store: StoreAdapter, signature: String) -> Self {
        Self { store, signature }
    }
}

impl KvBlobStore for TopologyStore {
    fn load<'a>(&mut self, key: u16, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
        self.store.load(key, buf)
    }

    fn store(&mut self, key: u16, data: &[u8], _buf: &mut [u8]) -> Result<(), Error> {
        self.store
            .check_failure()
            .map_err(|_| ErrorCode::StdIoError)?;
        if key != rs_matter::persist::BASIC_INFO_KEY {
            return self.store.record(self.store.storage().put(key, data));
        }
        self.store.record(
            self.store
                .storage()
                .save_topology(key, data, &self.signature),
        )
    }

    fn remove(&mut self, key: u16, buf: &mut [u8]) -> Result<(), Error> {
        self.store.remove(key, buf)
    }
}

#[cfg(test)]
mod tests;
