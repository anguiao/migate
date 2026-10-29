mod handlers;
mod lighting_handlers;
mod subscriptions;
pub(crate) mod support;

use support::*;

use super::{
    Bridge, PairingEvent,
    model::{basic_info, persist_basic_info, prepare_basic_info},
    pairing,
    storage::StoreAdapter,
};
use crate::{
    RuntimeError,
    device::{DeviceKey, FunctionKind, LightProfile, PhysicalDevice},
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
        model_handler(vec![single_entry(&test_device().0)]).access(|node| node
            .endpoints
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>()),
        [0, 1, 2]
    );
}

#[test]
fn plug_metadata_only_declares_basic_on_off_and_supported_events() {
    let model = model_handler(vec![single_entry(&test_device().0)]);
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
fn light_metadata_declares_only_real_level_temperature_and_xy_controls() {
    for rgb in [false, true] {
        let (device, _receiver) = support::light(rgb);
        let model = model_handler(vec![single_entry(&device)]);
        model.access(|node| {
            let endpoint = &node.endpoints[2];
            assert!(
                endpoint
                    .device_types
                    .iter()
                    .any(|t| t.dtype == if rgb { 0x10d } else { 0x10c } && t.drev == 4)
            );
            let level = endpoint.clusters.iter().find(|c| c.id == 8).unwrap();
            assert_eq!((level.feature_map, level.revision), (1, 6));
            let attributes = |cluster: &rs_matter::dm::Cluster<'_>| {
                cluster
                    .attributes
                    .iter()
                    .filter(|a| cluster.attribute(a.id).is_some())
                    .map(|a| a.id)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                attributes(level),
                [0, 1, 2, 3, 15, 17, 0xfff8, 0xfff9, 0xfffb, 0xfffc, 0xfffd]
            );
            let commands = |cluster: &rs_matter::dm::Cluster<'_>| {
                cluster
                    .commands
                    .iter()
                    .filter(|c| cluster.command(c.id).is_some())
                    .map(|c| c.id)
                    .collect::<Vec<_>>()
            };
            assert_eq!(commands(level), (0..8).collect::<Vec<_>>());
            for id in [0, 1, 2, 3, 15, 17] {
                assert!(level.attribute(id).is_some());
            }
            assert!(level.attribute(0x4000).is_none());
            for id in 0..8 {
                assert!(level.command(id).is_some());
            }
            assert!(level.command(8).is_none());
            let color = endpoint.clusters.iter().find(|c| c.id == 0x300).unwrap();
            assert_eq!(
                (color.feature_map, color.revision),
                (if rgb { 0x19 } else { 0x10 }, 9)
            );
            let mut expected = if rgb { vec![0, 1, 2, 3, 4] } else { vec![2] };
            expected.extend([
                7, 8, 15, 16, 0x4001, 0x400a, 0x400b, 0x400c, 0x400d, 0xfff8, 0xfff9, 0xfffb,
                0xfffc, 0xfffd,
            ]);
            assert_eq!(attributes(color), expected);
            assert_eq!(
                commands(color),
                if rgb {
                    vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x47, 0x4b, 0x4c]
                } else {
                    vec![10, 0x47, 0x4b, 0x4c]
                }
            );
            for id in [2, 7, 8, 15, 16, 0x4001, 0x400a, 0x400b, 0x400c, 0x400d] {
                assert!(color.attribute(id).is_some(), "{id}");
            }
            for id in [0x4000, 0x4010] {
                assert!(color.attribute(id).is_none());
            }
            for id in [0, 1, 3, 4] {
                assert_eq!(color.attribute(id).is_some(), rgb);
            }
            for id in [10, 0x47, 0x4b, 0x4c] {
                assert!(color.command(id).is_some());
            }
            for id in 0..=9 {
                assert_eq!(color.command(id).is_some(), rgb);
            }
            for id in [0x40, 0x41, 0x42, 0x43, 0x44] {
                assert!(color.command(id).is_none());
            }
            assert!(!endpoint.clusters.iter().any(|c| c.id == 0x62));
            let identify = endpoint.clusters.iter().find(|c| c.id == 3).unwrap();
            assert!(identify.command(0x40).is_none());
        });
    }
}

#[test]
fn on_off_and_dimmable_light_metadata_use_their_standard_device_types() {
    for (profile, device_type, has_level) in [
        (LightProfile::OnOff, 0x0100u16, false),
        (LightProfile::Dimmable, 0x0101u16, true),
    ] {
        let (device, _receiver) = support::light_with(profile);
        let model = model_handler(vec![single_entry(&device)]);
        model.access(|node| {
            let endpoint = &node.endpoints[2];
            assert!(
                endpoint
                    .device_types
                    .iter()
                    .any(|t| t.dtype == device_type && t.drev == 3)
            );
            assert!(endpoint.clusters.iter().any(|cluster| cluster.id == 6));
            assert_eq!(
                endpoint.clusters.iter().any(|cluster| cluster.id == 8),
                has_level
            );
            assert!(!endpoint.clusters.iter().any(|cluster| cluster.id == 0x300));
        });
    }
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
            let bridge = Bridge::new(
                vec![single_entry(&device)],
                1,
                &store,
                &identity,
                std::rc::Rc::new(NoLabels),
            );
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
fn corrupt_protocol_records_recover_without_changing_identity() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            for key in [
                rs_matter::persist::BASIC_INFO_KEY,
                rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START,
                rs_matter::persist::FABRIC_KEYS_START + 1,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::open(dir.path()).unwrap();
                store.matter().put(key, &[0xff, 0x11]).unwrap();
                let identity = store.load_identity().unwrap();
                let (device, _receiver) = test_device();
                let bridge = Bridge::new(
                    vec![single_entry(&device)],
                    7,
                    &store,
                    &identity,
                    std::rc::Rc::new(NoLabels),
                );
                let opened = std::cell::Cell::new(false);
                let result = block_on(futures_lite::future::poll_once(bridge.run(0, |event| {
                    if matches!(event, PairingEvent::Opened { .. }) {
                        opened.set(true);
                    }
                    Ok(())
                })));
                assert!(result.is_none(), "bridge must keep serving: {result:?}");
                assert!(opened.get(), "no valid fabric remains after recovery");
                bridge.check_failure().unwrap();
                assert_eq!(store.load_identity().unwrap(), identity);
                let settings = load_basic_info(&mut StoreAdapter::new(store.matter()));
                assert_eq!(settings.configuration_version, 7);
                assert_eq!(settings.node_label.as_str(), "MiGate");
                if key != rs_matter::persist::BASIC_INFO_KEY {
                    assert_eq!(store.matter().get(key).unwrap(), Some(vec![0xff, 0x11]));
                }
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
            let bridge = Bridge::new(
                vec![single_entry(&device)],
                1,
                &store,
                &identity,
                std::rc::Rc::new(NoLabels),
            );
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
fn committed_root_version_is_applied_before_startup_preserving_other_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let mut protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    let prepared = prepare_basic_info(&kv, 7).unwrap();
    assert_eq!(prepared.node_label.as_str(), "MiGate");
    assert_eq!(prepared.configuration_version, 7);
    persist_basic_info(&kv, &prepared).unwrap();
    assert_eq!(load_basic_info(&mut protocol), prepared);

    // An older blob catches up to the exact committed root; every other field
    // is preserved.
    let mut customized = prepared.clone();
    customized.node_label.clear();
    customized.node_label.push_str("Custom").unwrap();
    customized.local_config_disabled = true;
    customized.configuration_version = 5;
    rs_matter::persist::Persist::new(&kv)
        .store_tlv(BASIC_INFO_KEY, &customized)
        .unwrap();
    let prepared = prepare_basic_info(&kv, 9).unwrap();
    assert_eq!(prepared.node_label.as_str(), "Custom");
    assert!(prepared.local_config_disabled);
    assert_eq!(prepared.configuration_version, 9);
    persist_basic_info(&kv, &prepared).unwrap();
    let settings = load_basic_info(&mut protocol);
    assert_eq!(settings.node_label.as_str(), "Custom");
    assert!(settings.local_config_disabled);
    assert_eq!(settings.configuration_version, 9);

    // A blob newer than the committed root is corruption and is never
    // overwritten or silently clamped; a rejected prepare persists nothing.
    let mut newer = settings.clone();
    newer.configuration_version = 12;
    rs_matter::persist::Persist::new(&kv)
        .store_tlv(BASIC_INFO_KEY, &newer)
        .unwrap();
    assert!(prepare_basic_info(&kv, 10).is_err());
    assert_eq!(load_basic_info(&mut protocol).configuration_version, 12);
}

fn load_basic_info(
    protocol: &mut StoreAdapter,
) -> rs_matter::dm::clusters::basic_info::BasicInfoSettings {
    let mut buf = [0; 1024];
    let blob = protocol.load(BASIC_INFO_KEY, &mut buf).unwrap().unwrap();
    rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(blob))
        .unwrap()
}
mod sensors;

mod curtain;

mod fan;
