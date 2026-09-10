mod handlers;
mod subscriptions;

use super::{
    NODE, basic_info, bridged_info, initialize_basic_info, kv::ProtocolStore, pairing_codes, run,
};
use crate::{storage::Store, virtual_device::VirtualLight};
use rs_matter::{
    MATTER_PORT, Matter,
    dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM},
    persist::{BASIC_INFO_KEY, KvBlobStore},
    tlv::{FromTLV, TLVElement},
};

#[test]
fn topology_and_identity_are_fixed() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let info = basic_info(store.identity());
    assert_eq!(info.serial_no, store.identity().bridge_id);
    assert_eq!(info.unique_id, store.identity().bridge_id);
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
    let info = basic_info(store.identity());
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
                let mut store = Store::open(dir.path()).unwrap();
                store.store(key, &[0xff, 0x11]).unwrap();
                let before = std::fs::read(dir.path().join("state.json")).unwrap();
                let light = VirtualLight::new();
                let error =
                    futures_lite::future::block_on(run(&light, store, std::future::pending()))
                        .unwrap_err();
                assert!(error.to_string().contains(dir.path().to_str().unwrap()));
                assert_eq!(
                    std::fs::read(dir.path().join("state.json")).unwrap(),
                    before
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn default_node_label_uses_upstream_format_and_preserves_existing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.identity().clone();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let mut protocol = ProtocolStore::new(store);
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
