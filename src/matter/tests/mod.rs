mod handlers;
mod subscriptions;

use super::{
    Bridge, PairingEvent,
    model::{self, basic_info, initialize_basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::{Capability, Category, CommandReceiver, Device, DeviceDescription, DeviceKey},
    storage::Store,
};
use futures_lite::future::{block_on, or};
use rs_matter::{
    MATTER_PORT, Matter,
    dm::{
        Metadata,
        devices::test::{TEST_DEV_ATT, TEST_DEV_COMM},
    },
    persist::{BASIC_INFO_KEY, KvBlobStore},
    tlv::{FromTLV, TLVElement},
};

pub(super) fn physical(
    id: &str,
    multi: bool,
) -> (crate::device::PhysicalDevice, Vec<CommandReceiver>) {
    let functions = if multi {
        vec![
            ("outlet_1", "开关 1"),
            ("outlet_2", "开关 2"),
            ("usb", "USB"),
        ]
    } else {
        vec![("main", "主电源")]
    };
    crate::device::PhysicalDevice::new(crate::device::PhysicalDeviceDescription {
        key: DeviceKey::new("xiaomi", id),
        label: format!("Outlet {id}"),
        functions: functions
            .into_iter()
            .map(|(id, label)| crate::device::FunctionDescription {
                id: id.into(),
                label: label.into(),
                category: Category::Outlet,
                capabilities: vec![Capability::OnOff],
            })
            .collect(),
    })
}
pub(crate) fn test_registry(store: &Store, device: &Device) -> crate::device::DeviceRegistry {
    let physical = crate::device::PhysicalDevice::from_single(device.clone(), "Test Outlet".into());
    store.devices().publish(physical.description()).unwrap();
    let registry = crate::device::DeviceRegistry::new();
    registry.insert(physical);
    registry
}
pub(crate) fn test_device() -> (Device, CommandReceiver) {
    Device::new(DeviceDescription {
        key: DeviceKey::new("xiaomi", "test-outlet"),
        category: Category::Outlet,
        capabilities: vec![Capability::OnOff],
    })
}
pub(super) async fn terminal_command(device: &Device, line: &str) -> Option<String> {
    use crate::{
        terminal::{self, AuthStatus},
        xiaomi::auth::{AuthReport, AuthenticationState, CertificateUpdate},
    };
    let report = AuthReport::for_test(
        AuthenticationState::NotSignedIn,
        None,
        CertificateUpdate::NotNeeded,
        0,
    );
    let status = AuthStatus::new(report, "/bin/migate".into(), "/data".into());
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let registry = test_registry(&store, device);
    let line = if matches!(line, "on" | "off") {
        format!("{line} {}", device.description().key.id)
    } else {
        line.into()
    };
    terminal::handle_line(&registry, &store.devices(), &status, &line, 0)
        .await
        .unwrap()
}

fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = test_device();
    let registry = test_registry(store, &device);
    let bridge = Bridge::new(&registry, store, &identity);
    block_on(or(bridge.run(0, |_| Ok(())), async {
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
        model::handler(
            &test_registry(&store, &test_device().0),
            &store,
            StoreAdapter::new(store.matter()),
            rand::rng()
        )
        .unwrap()
        .access(|node| node.endpoints.iter().map(|e| e.id).collect::<Vec<_>>()),
        [0, 1, 2]
    );
}

#[test]
fn plug_metadata_only_declares_basic_on_off_and_supported_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let model = model::handler(
        &test_registry(&store, &test_device().0),
        &store,
        StoreAdapter::new(store.matter()),
        rand::rng(),
    )
    .unwrap();
    model.access(|node| {
        let endpoint = &node.endpoints[2];
        assert!(
            endpoint
                .device_types
                .iter()
                .any(|kind| kind.dtype == 0x010a && kind.drev == 3)
        );
        assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 0x62));
        let on_off = endpoint
            .clusters
            .iter()
            .find(|cluster| cluster.id == 6)
            .unwrap();
        assert_eq!(on_off.feature_map, 0);
        assert!(on_off.attribute(0).is_some());
        for id in 0x4000..=0x4003 {
            assert!(on_off.attribute(id).is_none());
        }
        for id in 0..=2 {
            assert!(on_off.command(id).is_some());
        }
        for id in 0x40..=0x42 {
            assert!(on_off.command(id).is_none());
        }
        let bridged = endpoint
            .clusters
            .iter()
            .find(|cluster| cluster.id == 0x39)
            .unwrap();
        assert!(bridged.event(3).is_some());
        for id in [0, 1, 2, 0x80] {
            assert!(bridged.event(id).is_none());
        }
        let identify = endpoint
            .clusters
            .iter()
            .find(|cluster| cluster.id == 3)
            .unwrap();
        assert!(identify.command(0x40).is_some());
        let groups = endpoint
            .clusters
            .iter()
            .find(|cluster| cluster.id == 4)
            .unwrap();
        for id in 0..=5 {
            assert!(groups.command(id).is_some());
        }
    });
}

#[test]
fn unused_legacy_scenes_blob_is_preserved_without_loading() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let key = rs_matter::persist::SCENES_KEY;
            store.matter().put(key, &[0xff, 0x11]).unwrap();
            let identity = store.load_identity().unwrap();
            let (device, _receiver) = test_device();
            let registry = test_registry(&store, &device);
            let bridge = Bridge::new(&registry, &store, &identity);
            let error =
                block_on(bridge.run(0, |_| Err(std::io::Error::other("startup completed"))))
                    .unwrap_err();
            assert_eq!(error.to_string(), "startup completed");
            bridge.check_failure().unwrap();
            assert_eq!(
                Store::open(dir.path()).unwrap().matter().get(key).unwrap(),
                Some(vec![0xff, 0x11])
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn pairing_codes_use_the_same_passcode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let PairingEvent::Opened {
        qr_payload: qr,
        manual_code: manual,
        ..
    } = pairing::opened(&info, 900).unwrap()
    else {
        panic!("expected pairing information");
    };
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
            let (device, _receiver) = test_device();
            let registry = test_registry(&store, &device);
            let bridge = Bridge::new(&registry, &store, &identity);
            let running = bridge.run(0, |_| Ok(()));
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
