use super::*;
use crate::device::{AccountId, DeviceDid, FeatureIdentity, FeatureRole, HomeId, PhysicalDeviceId};

fn feature(service_instance: u32) -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("u").unwrap(),
            home: HomeId::new("h").unwrap(),
            parent_did: DeviceDid::new("d").unwrap(),
        },
        service_instance,
        role: FeatureRole::Light,
    }
}

#[test]
fn endpoint_scenes_are_isolated_bounded_and_restored() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let first = store
        .devices()
        .allocate_feature(&feature(2))
        .unwrap()
        .endpoint;
    let second = store
        .devices()
        .allocate_feature(&feature(3))
        .unwrap()
        .endpoint;
    store
        .matter()
        .save_endpoint_scenes(first, b"first")
        .unwrap();
    store
        .matter()
        .save_endpoint_scenes(second, b"second")
        .unwrap();
    assert!(
        store
            .matter()
            .save_endpoint_scenes(first, &[0; 4097])
            .is_err()
    );
    drop(store);

    let reopened = Store::open(directory.path()).unwrap();
    assert_eq!(
        reopened.matter().endpoint_scenes(first).unwrap().as_deref(),
        Some(b"first".as_slice())
    );
    assert_eq!(
        reopened
            .matter()
            .endpoint_scenes(second)
            .unwrap()
            .as_deref(),
        Some(b"second".as_slice())
    );
    reopened.matter().delete_endpoint_scenes(first).unwrap();
    assert_eq!(reopened.matter().endpoint_scenes(first).unwrap(), None);
    assert_eq!(
        reopened
            .matter()
            .endpoint_scenes(second)
            .unwrap()
            .as_deref(),
        Some(b"second".as_slice())
    );
}

#[test]
fn topology_blob_and_signature_commit_or_roll_back_together() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let matter = store.matter();
    matter.put(1, b"old").unwrap();
    let observer = rusqlite::Connection::open(store.path()).unwrap();
    observer
        .execute_batch(
            "CREATE TRIGGER reject_topology BEFORE UPDATE ON matter_topology
             BEGIN SELECT RAISE(FAIL, 'rejected'); END;",
        )
        .unwrap();

    assert!(matter.save_topology(1, b"new", &"1".repeat(40)).is_err());
    assert_eq!(matter.get(1).unwrap().as_deref(), Some(b"old".as_slice()));
    assert_eq!(matter.topology_signature().unwrap(), "");
}

#[test]
fn missing_topology_singleton_is_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let observer = rusqlite::Connection::open(store.path()).unwrap();
    observer.execute("DELETE FROM matter_topology", []).unwrap();

    assert!(store.matter().topology_signature().is_err());
    assert!(
        store
            .matter()
            .save_topology(1, b"blob", &"1".repeat(40))
            .is_err()
    );
    assert_eq!(store.matter().get(1).unwrap(), None);
}

#[test]
fn corrupt_topology_metadata_is_rejected_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    db.execute("UPDATE matter_topology SET signature = 'not-a-digest'", [])
        .unwrap();
    db.execute(
        "INSERT INTO feature_identities (
                account_uid,home_id,parent_did,service_instance,role,endpoint,public_id,active
             ) VALUES ('u','h','d',2,'temperature_sensor',2,?1,1)",
        ["1".repeat(32)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO matter_feature_labels (endpoint,label) VALUES (2,?1)",
        ["x".repeat(33)],
    )
    .unwrap();

    assert!(store.matter().topology_signature().is_err());
    assert!(store.matter().feature_label(2).is_err());
}
