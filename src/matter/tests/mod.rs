mod composition;
mod config_version;
pub(in crate::matter) mod handlers;
mod publication;
mod subscriptions;
pub(crate) mod support;

use support::*;

/// A registry with the device installed **and applied to the published set**.
/// The Matter model publishes from the applied configuration, mirroring
/// bootstrap's restore path; this helper also publishes the stored identity.
/// `support::test_registry` (owned by the terminal slice) only installs the
/// runtime device.
pub(in crate::matter) fn published_registry(
    store: &Store,
    device: &PhysicalDevice,
) -> crate::device::DeviceRegistry {
    let registry = test_registry(store, device);
    registry
        .apply_configuration(0, Vec::new(), &[device.key().clone()], &[])
        .unwrap();
    registry
}

use super::{
    Bridge, PairingEvent,
    model::{self, basic_info, initialize_basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::{Capability, Category, CommandReceiver, DeviceKey, PhysicalDevice},
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
            &published_registry(&store, &test_device().0),
            &store,
            StoreAdapter::new(store.matter()),
            test_executor(),
            rand::rng(),
            None
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
        &published_registry(&store, &test_device().0),
        &store,
        StoreAdapter::new(store.matter()),
        test_executor(),
        rand::rng(),
        None,
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
fn light_metadata_matches_each_real_capability_combination() {
    let range = crate::device::NumericRange {
        min: 2700,
        max: 6500,
        step: 1,
    };
    // on_off, brightness, color temperature, rgb
    for (on_off, brightness, temperature, rgb) in [
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
        (false, true, true, false),
        (false, true, false, true),
        (false, false, true, true),
        (false, true, true, true),
        (true, false, false, false),
        (true, true, false, false),
        (true, false, true, false),
        (true, false, false, true),
        (true, true, true, false),
        (true, true, false, true),
        (true, false, true, true),
        (true, true, true, true),
    ] {
        let mut capabilities = Vec::new();
        if on_off {
            capabilities.push(Capability::OnOff);
        }
        if brightness {
            capabilities.push(Capability::Brightness);
        }
        if temperature {
            capabilities.push(Capability::ColorTemperature(range));
        }
        if rgb {
            capabilities.push(Capability::ColorRgb);
        }
        let (device, _receiver) = support::light_caps("test-light", &capabilities);
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let model = model::handler(
            &published_registry(&store, &device),
            &store,
            StoreAdapter::new(store.matter()),
            test_executor(),
            rand::rng(),
            None,
        )
        .unwrap();
        let combo = (on_off, brightness, temperature, rgb);
        model.access(|node| {
            if !on_off {
                assert_eq!(node.endpoints.len(), 2, "{combo:?}");
                return;
            }
            assert_light_surface(
                &node.endpoints[2],
                brightness,
                temperature,
                rgb,
                true,
                combo,
            );
        });
    }
}

pub(in crate::matter) fn assert_light_surface(
    endpoint: &rs_matter::dm::Endpoint<'_>,
    brightness: bool,
    temperature: bool,
    rgb: bool,
    bridged: bool,
    combo: (bool, bool, bool, bool),
) {
    let (dtype, drev) = match (brightness, temperature, rgb) {
        (false, _, _) => (0x100, 3),
        (true, true, true) => (0x10d, 4),
        (true, true, false) => (0x10c, 4),
        (true, _, _) => (0x101, 3),
    };
    assert!(
        endpoint
            .device_types
            .iter()
            .any(|t| t.dtype == dtype && t.drev == drev),
        "{combo:?}"
    );
    assert_eq!(
        endpoint.device_types.iter().any(|t| t.dtype == 0x0013),
        bridged,
        "{combo:?}"
    );
    let ids = endpoint
        .clusters
        .iter()
        .map(|cluster| cluster.id)
        .collect::<Vec<_>>();
    for (id, present) in [
        (0x1d, true),
        (4, true),
        (3, true),
        (0x39, bridged),
        (6, true),
        (8, brightness),
        (0x300, temperature || rgb),
    ] {
        assert_eq!(ids.contains(&id), present, "{combo:?} cluster {id:#x}");
    }
    if brightness {
        let level = endpoint.clusters.iter().find(|c| c.id == 8).unwrap();
        assert_eq!((level.feature_map, level.revision), (1, 6), "{combo:?}");
        assert_eq!(
            attributes(level),
            [0, 1, 2, 3, 15, 17, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd]
        );
        assert_eq!(commands(level), (0..8).collect::<Vec<_>>());
    }
    if temperature || rgb {
        let color = endpoint.clusters.iter().find(|c| c.id == 0x300).unwrap();
        let feature_map = (if temperature { 0x10 } else { 0 }) | (if rgb { 0x09 } else { 0 });
        assert_eq!(
            (color.feature_map, color.revision),
            (feature_map, 9),
            "{combo:?}"
        );
        let mut expected = Vec::new();
        if rgb {
            expected.extend([0, 1]);
        }
        expected.push(2);
        if rgb {
            expected.extend([3, 4]);
        }
        if temperature {
            expected.push(7);
        }
        expected.extend([8, 15, 16, 0x4001, 0x400a]);
        if temperature {
            expected.extend([0x400b, 0x400c, 0x400d]);
        }
        expected.extend([0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd]);
        assert_eq!(attributes(color), expected, "{combo:?}");
        let mut expected_commands = Vec::new();
        if rgb {
            expected_commands.extend(0..=9);
        }
        if temperature {
            expected_commands.push(10);
        }
        expected_commands.extend(
            [0x47, 0x4b, 0x4c]
                .into_iter()
                .filter(|id| temperature || *id == 0x47),
        );
        assert_eq!(commands(color), expected_commands, "{combo:?}");
        for id in [0x4000, 0x4010] {
            assert!(color.attribute(id).is_none(), "{combo:?} attribute {id:#x}");
        }
        if !temperature {
            for id in [7, 0x400b, 0x400c, 0x400d] {
                assert!(
                    color.attribute(id).is_none(),
                    "{combo:?} CT attribute {id:#x}"
                );
            }
        }
        if !rgb {
            for id in [0, 1, 3, 4] {
                assert!(
                    color.attribute(id).is_none(),
                    "{combo:?} RGB attribute {id:#x}"
                );
            }
        }
    }
    assert!(!endpoint.clusters.iter().any(|c| c.id == 0x62), "{combo:?}");
    let identify = endpoint.clusters.iter().find(|c| c.id == 3).unwrap();
    assert!(identify.command(0x40).is_none(), "{combo:?}");
}

fn attributes(cluster: &rs_matter::dm::Cluster<'_>) -> Vec<u32> {
    cluster
        .attributes
        .iter()
        .filter(|a| cluster.attribute(a.id).is_some())
        .map(|a| a.id)
        .collect()
}

fn commands(cluster: &rs_matter::dm::Cluster<'_>) -> Vec<u32> {
    cluster
        .commands
        .iter()
        .filter(|c| cluster.command(c.id).is_some())
        .map(|c| c.id)
        .collect()
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
            let registry = published_registry(&store, &device);
            let bridge = Bridge::new(&registry, &store, &identity, test_executor());
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
