mod device_bridge;
mod handlers;
mod subscriptions;

use super::{DeviceBridge, PairingEvent, common, pairing, storage::StoreAdapter};
use crate::{RuntimeError, device::DeviceService, storage::Store};
use futures_lite::future::{block_on, or};
use rs_matter::{
    MATTER_PORT, Matter,
    dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM},
    persist::{BASIC_INFO_KEY, KvBlobStore},
    tlv::{FromTLV, TLVElement},
};

fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let service = DeviceService::new();
    let bridge = DeviceBridge::new(&service, &identity, store.clone());
    block_on(or(bridge.run(0, |_| Ok(())), async {
        async_io::Timer::after(std::time::Duration::from_secs(5)).await;
        panic!("startup did not report invalid storage");
    }))
    .unwrap_err()
}

#[test]
fn zero_device_topology_and_identity_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = common::basic_info(&identity);
    assert_eq!(info.serial_no, identity.bridge_id);
    assert_eq!(info.unique_id, identity.bridge_id);
    assert_eq!(info.product_name, "MiGate");
    assert_eq!(
        common::BASE_NODE
            .endpoints
            .iter()
            .map(|endpoint| endpoint.id)
            .collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn pairing_codes_use_the_same_passcode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = common::basic_info(&identity);
    let PairingEvent::Opened {
        qr_payload: qr,
        manual_code: manual,
        ..
    } = pairing::opened(&info, 900).unwrap()
    else {
        panic!("expected pairing information");
    };
    let mut buffer = [0; 1024];
    let qr = rs_matter::pairing::qr::QrPayload::parse(&qr, &mut buffer).unwrap();
    let manual = rs_matter::pairing::qr::QrPayload::parse_pairing_code(&manual).unwrap();
    assert_eq!(qr.passcode(), manual.passcode());
}

#[test]
fn protocol_corruption_exits_with_path_without_overwriting() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            for key in [
                BASIC_INFO_KEY,
                rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START,
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
                let database = rusqlite::Connection::open(reopened.path()).unwrap();
                let blobs = database
                    .prepare("SELECT key, value FROM blobs ORDER BY key")
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
fn corrupt_model_data_is_rejected_before_pairing_is_announced() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .matter()
        .put(
            rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START,
            &[0xff, 0x11],
        )
        .unwrap();
    let identity = store.load_identity().unwrap();
    let service = DeviceService::new();
    let bridge = DeviceBridge::new(&service, &identity, store);
    let pairing_announced = std::cell::Cell::new(false);
    let error = block_on(bridge.run(0, |_| {
        pairing_announced.set(true);
        Ok(())
    }))
    .unwrap_err();
    assert!(!pairing_announced.get());
    assert!(error.source().unwrap().is::<rs_matter::error::Error>());
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
            let database = rusqlite::Connection::open(store.path()).unwrap();
            database
                .execute_batch(
                    "ALTER TABLE blobs RENAME TO stored_blobs;
                     CREATE VIEW blobs AS SELECT key, 'SECRET CONTENT' AS value FROM stored_blobs;",
                )
                .unwrap();

            let error = startup_error(&store);
            let storage = error
                .downcast_ref::<crate::storage::StorageError>()
                .unwrap();
            assert_eq!(storage.path(), store.path());
            assert_eq!(
                storage.operation(),
                format!("read Matter data for key {BASIC_INFO_KEY}")
            );
            assert!(matches!(
                error.source().unwrap().downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::InvalidColumnType(..))
            ));
            assert!(!format!("{error:?} {error}").contains("SECRET CONTENT"));
            assert_eq!(
                database
                    .query_row(
                        "SELECT value FROM stored_blobs WHERE key=?1",
                        [BASIC_INFO_KEY],
                        |row| row.get::<_, Vec<u8>>(0),
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
fn cancelled_bridge_keeps_a_recorded_storage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let service = DeviceService::new();
    let bridge = DeviceBridge::new(&service, &identity, store.clone());
    let mut callback_store = bridge.store_for_test();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute("ALTER TABLE blobs RENAME TO unavailable_blobs", [])
        .unwrap();
    assert!(callback_store.store(1, b"value", &mut []).is_err());
    let running = bridge.run(0, |_| Ok(()));
    block_on(or(async { Ok::<(), RuntimeError>(()) }, running)).unwrap();
    let error = bridge.check_failure().unwrap_err();
    assert_eq!(error.path(), store.path());
    assert_eq!(error.operation(), "write Matter data for key 1");
}

#[test]
fn default_node_label_uses_upstream_format_and_preserves_existing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = common::basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let mut protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    common::initialize_basic_info(&matter, &kv, true).unwrap();
    let mut buffer = [0; 1024];
    let before = protocol
        .load(BASIC_INFO_KEY, &mut buffer)
        .unwrap()
        .unwrap()
        .to_vec();
    let settings =
        rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&before))
            .unwrap();
    assert_eq!(settings.node_label.as_str(), "MiGate");
    common::initialize_basic_info(&matter, &kv, false).unwrap();
    assert_eq!(
        protocol.load(BASIC_INFO_KEY, &mut buffer).unwrap().unwrap(),
        before
    );
}
