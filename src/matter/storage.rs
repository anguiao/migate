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
mod tests {
    use super::*;
    use crate::storage::Store;
    use std::error::Error as _;

    #[test]
    fn raw_blob_roundtrip_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut kv = StoreAdapter::new(store.matter());
        kv.store(123, &[1, 2, 3], &mut []).unwrap();
        let mut restored = StoreAdapter::new(Store::open(dir.path()).unwrap().matter());
        let mut buf = [0; 10];
        assert_eq!(
            restored.load(123, &mut buf[..2]).unwrap_err().code(),
            ErrorCode::NoSpace
        );
        restored.check_failure().unwrap();
        assert_eq!(restored.load(123, &mut buf).unwrap(), Some(&[1, 2, 3][..]));
        restored.remove(123, &mut []).unwrap();
        assert_eq!(restored.load(123, &mut buf).unwrap(), None);
        assert!(!store.matter().contains(123).unwrap());
    }

    #[test]
    fn database_failures_reach_fatal_watcher() {
        for (operation, context) in [
            ("load", "read Matter data for key 1"),
            ("store", "write Matter data for key 1"),
            ("remove", "delete Matter data for key 1"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut kv = StoreAdapter::new(Store::open(dir.path()).unwrap().matter());
            let db = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
            db.execute("ALTER TABLE blobs RENAME TO unavailable_blobs", [])
                .unwrap();
            let result = match operation {
                "load" => kv.load(1, &mut [0; 10]).map(|_| ()),
                "store" => kv.store(1, &[1], &mut []),
                _ => kv.remove(1, &mut []),
            };
            assert_eq!(
                result.unwrap_err().code(),
                ErrorCode::StdIoError,
                "{operation}"
            );
            let error =
                futures_lite::future::block_on(futures_lite::future::poll_once(kv.wait_failure()))
                    .expect("storage failure must be recorded");
            assert_eq!(error.path(), dir.path().join("state.db"));
            assert_eq!(error.operation(), context);
            assert!(error.source().unwrap().is::<rusqlite::Error>());
            let startup_error = kv.with_context("restore Matter data", Ok(())).unwrap_err();
            assert_eq!(startup_error.operation(), context);
        }
    }

    #[test]
    fn observed_failure_remains_sticky_and_stops_future_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = StoreAdapter::new(Store::open(dir.path()).unwrap().matter());
        store.store(1, &[1], &mut []).unwrap();
        let db = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
        db.execute("ALTER TABLE blobs RENAME TO unavailable_blobs", [])
            .unwrap();
        assert!(store.store(1, &[2], &mut []).is_err());
        db.execute("ALTER TABLE unavailable_blobs RENAME TO blobs", [])
            .unwrap();
        let error = store.check_failure().unwrap_err();
        assert_eq!(error.operation(), "write Matter data for key 1");
        assert!(store.store(2, &[2], &mut []).is_err());
        assert!(store.remove(1, &mut []).is_err());
        assert!(store.load(1, &mut [0; 10]).is_err());
        let reopened = Store::open(dir.path()).unwrap().matter();
        assert_eq!(reopened.get(1).unwrap(), Some(vec![1]));
        assert_eq!(reopened.get(2).unwrap(), None);
    }

    #[test]
    fn topology_transaction_failure_is_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let adapter = StoreAdapter::new(store.matter());
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_topology BEFORE UPDATE ON matter_topology
             BEGIN SELECT RAISE(FAIL, 'rejected'); END;",
        )
        .unwrap();
        let mut topology = TopologyStore::new(adapter.clone(), "1".repeat(40));
        assert!(
            topology
                .store(rs_matter::persist::BASIC_INFO_KEY, b"new", &mut [])
                .is_err()
        );
        db.execute("DROP TRIGGER reject_topology", []).unwrap();

        assert_eq!(
            adapter.check_failure().unwrap_err().operation(),
            "write Matter topology signature"
        );
        assert!(
            topology
                .store(rs_matter::persist::BASIC_INFO_KEY, b"new", &mut [])
                .is_err()
        );
        assert_eq!(
            store
                .matter()
                .get(rs_matter::persist::BASIC_INFO_KEY)
                .unwrap(),
            None
        );
    }
}
