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
