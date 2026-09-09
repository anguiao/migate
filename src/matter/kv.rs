use crate::storage::{StorageError, Store};
use event_listener::Event;
use rs_matter::{
    error::{Error, ErrorCode},
    persist::KvBlobStore,
};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
pub(super) struct ProtocolStore(Rc<Inner>);
struct Inner {
    store: RefCell<Store>,
    failure: RefCell<Option<Rc<StorageError>>>,
    failed: Event,
}
impl ProtocolStore {
    pub fn new(store: Store) -> Self {
        Self(Rc::new(Inner {
            store: RefCell::new(store),
            failure: RefCell::new(None),
            failed: Event::new(),
        }))
    }
    fn record(&self, result: Result<(), StorageError>) -> Result<(), Error> {
        result.map_err(|error| {
            let mut failure = self.0.failure.borrow_mut();
            if failure.is_none() {
                *failure = Some(Rc::new(error));
            }
            self.0.failed.notify(usize::MAX);
            ErrorCode::StdIoError.into()
        })
    }
    pub async fn failed(&self) -> Rc<StorageError> {
        loop {
            let listener = self.0.failed.listen();
            if let Some(error) = self.0.failure.borrow().clone() {
                return error;
            }
            listener.await;
        }
    }
    pub fn flush(&self) -> Result<(), Rc<StorageError>> {
        if let Some(error) = self.0.failure.borrow().clone() {
            return Err(error);
        }
        self.0.store.borrow().flush().map_err(Rc::new)
    }
    fn healthy(&self) -> Result<(), Error> {
        if self.0.failure.borrow().is_some() {
            Err(ErrorCode::StdIoError.into())
        } else {
            Ok(())
        }
    }
}
impl KvBlobStore for ProtocolStore {
    fn load<'a>(&mut self, key: u16, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
        self.healthy()?;
        let store = self.0.store.borrow();
        let Some(data) = store.load(key) else {
            return Ok(None);
        };
        let target = buf.get_mut(..data.len()).ok_or(ErrorCode::NoSpace)?;
        target.copy_from_slice(data);
        Ok(Some(target))
    }
    fn store(&mut self, key: u16, data: &[u8], _buf: &mut [u8]) -> Result<(), Error> {
        self.healthy()?;
        self.record(self.0.store.borrow_mut().store(key, data))
    }
    fn remove(&mut self, key: u16, _buf: &mut [u8]) -> Result<(), Error> {
        self.healthy()?;
        self.record(self.0.store.borrow_mut().remove(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observed_failure_remains_sticky_and_stops_future_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut store = ProtocolStore::new(Store::open(&path).unwrap());
        std::fs::remove_dir_all(&path).unwrap();
        assert!(store.store(1, &[1], &mut []).is_err());
        let _ = futures_lite::future::block_on(store.failed());
        std::fs::create_dir(&path).unwrap();
        assert!(super::super::finish(&store, Ok(())).is_err());
        assert!(store.flush().is_err());
        assert!(store.store(2, &[2], &mut []).is_err());
        assert!(!path.join("state.json").exists());
    }
}
