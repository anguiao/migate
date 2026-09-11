mod handlers;
mod subscriptions;

use super::{
    Bridge, NODE, basic_info, bridged_info, initialize_basic_info, pairing_codes,
    storage::StoreAdapter,
};
use crate::{RuntimeError, storage::Store, virtual_device::VirtualLight};
use futures_lite::future::{block_on, or};
use rs_matter::{
    MATTER_PORT, Matter,
    dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM},
    persist::{BASIC_INFO_KEY, KvBlobStore},
    tlv::{FromTLV, TLVElement},
};

fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let light = VirtualLight::new();
    let bridge = Bridge::new(&light, &identity, store.matter());
    block_on(or(bridge.run(), async {
        async_io::Timer::after(std::time::Duration::from_secs(5)).await;
        panic!("startup did not report invalid storage");
    }))
    .unwrap_err()
}

#[test]
fn topology_and_identity_are_fixed() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    assert_eq!(info.serial_no, identity.bridge_id);
    assert_eq!(info.unique_id, identity.bridge_id);
    assert_eq!(info.product_name, "MiGate");
    assert_eq!(
        NODE.endpoints.iter().map(|e| e.id).collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

#[test]
fn pairing_codes_use_the_same_passcode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let (qr, manual) = pairing_codes(&info).unwrap();
    let mut buf = [0; 1024];
    let qr = rs_matter::pairing::qr::QrPayload::parse(&qr, &mut buf).unwrap();
    let manual = rs_matter::pairing::qr::QrPayload::parse_pairing_code(&manual).unwrap();
    assert_eq!(qr.passcode(), manual.passcode());
}

#[test]
fn protocol_corruption_exits_with_path_without_overwriting() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            for key in [
                rs_matter::persist::BASIC_INFO_KEY,
                rs_matter::persist::SCENES_KEY,
                rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START,
                bridged_info::LABEL_KEY,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::open(dir.path()).unwrap();
                store.matter().put(key, &[0xff, 0x11]).unwrap();
                let identity = store.load_identity().unwrap();
                let error = startup_error(&store);
                assert!(error.to_string().contains(dir.path().to_str().unwrap()));
                assert!(error.source().unwrap().is::<rs_matter::error::Error>());
                let reopened = Store::open(dir.path()).unwrap();
                assert_eq!(reopened.load_identity().unwrap(), identity);
                let db = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
                let blobs = db
                    .prepare("SELECT key, value FROM blobs")
                    .unwrap()
                    .query_map([], |row| {
                        Ok((row.get::<_, u16>(0)?, row.get::<_, Vec<u8>>(1)?))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(blobs, [(key, vec![0xff, 0x11])]);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn protocol_startup_preserves_the_original_database_failure() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let identity = store.load_identity().unwrap();
            store.matter().put(BASIC_INFO_KEY, b"original").unwrap();
            let db = rusqlite::Connection::open(store.path()).unwrap();
            // Existence checks succeed, but the protocol's subsequent blob read fails.
            db.execute_batch(
                "ALTER TABLE blobs RENAME TO stored_blobs;
                CREATE VIEW blobs AS SELECT key, 'SECRET CONTENT' AS value FROM stored_blobs;",
            )
            .unwrap();

            let error = startup_error(&store);
            let storage_error = error
                .downcast_ref::<crate::storage::StorageError>()
                .unwrap();
            assert_eq!(storage_error.path(), store.path());
            assert_eq!(
                storage_error.operation(),
                format!("read Matter data for key {BASIC_INFO_KEY}")
            );
            assert!(matches!(
                error.source().unwrap().downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::InvalidColumnType(..))
            ));
            assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
            assert_eq!(
                db.query_row(
                    "SELECT value FROM stored_blobs WHERE key = ?1",
                    [BASIC_INFO_KEY],
                    |row| row.get::<_, Vec<u8>>(0)
                )
                .unwrap(),
                b"original"
            );
            assert_eq!(store.load_identity().unwrap(), identity);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn cancelled_run_keeps_a_recorded_storage_failure() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let light = VirtualLight::new();
            let bridge = Bridge::new(&light, &identity, store.matter());
            let running = bridge.run();
            let mut callback_store = bridge.store.clone();
            let db = rusqlite::Connection::open(store.path()).unwrap();
            db.execute("ALTER TABLE blobs RENAME TO unavailable_blobs", [])
                .unwrap();
            assert!(callback_store.store(1, b"value", &mut []).is_err());
            drop(callback_store);

            // Let application shutdown win before the run future can report the callback failure.
            block_on(or(async { Ok::<(), RuntimeError>(()) }, running)).unwrap();
            let error = bridge.check_failure().unwrap_err();
            assert_eq!(error.path(), store.path());
            assert_eq!(error.operation(), "write Matter data for key 1");
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn default_node_label_uses_upstream_format_and_preserves_existing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let mut protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    initialize_basic_info(&matter, &kv, true).unwrap();
    let mut buf = [0; 1024];
    let before = protocol
        .load(BASIC_INFO_KEY, &mut buf)
        .unwrap()
        .unwrap()
        .to_vec();
    let settings =
        rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&before))
            .unwrap();
    assert_eq!(settings.node_label.as_str(), "MiGate");
    initialize_basic_info(&matter, &kv, false).unwrap();
    assert_eq!(
        protocol.load(BASIC_INFO_KEY, &mut buf).unwrap().unwrap(),
        before
    );
}
