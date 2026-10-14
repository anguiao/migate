use super::support::stage_configuration;
use crate::{
    device::{Capability, Category, Composition, DeviceKey, FunctionDescription},
    storage::{CachedPhysicalDevice, CachedScope, Store},
    xiaomi::{
        capability::{ParsedDevice, ParsedFunction, PropertyBinding},
        cloud::group_id,
    },
};

fn cached_light(uid: &str, did: &str) -> CachedPhysicalDevice {
    CachedPhysicalDevice {
        uid: uid.into(),
        home_id: "local".into(),
        group_id: group_id(uid, "local"),
        model: "yeelink.light.light3".into(),
        instance_type: "urn:miot-spec-v2:device:light:0000A001:yeelink-light3:1:0000C802".into(),
        device: ParsedDevice {
            key: DeviceKey::new("xiaomi", did),
            functions: vec![ParsedFunction {
                description: FunctionDescription {
                    id: "main".into(),
                    label: "Light".into(),
                    category: Category::Light,
                    capabilities: vec![Capability::OnOff],
                },
                power: Some(PropertyBinding { siid: 2, piid: 1 }),
                lighting: None,
            }],
            refresh_trigger: None,
        },
    }
}

fn compose(store: &Store) -> (DeviceKey, u16) {
    let scope = CachedScope {
        uid: "uid".into(),
        home_id: "local".into(),
        group_id: group_id("uid", "local"),
    };
    let member = cached_light("uid", "member-one");
    store
        .devices()
        .publish_cached(&member, "Member One")
        .unwrap();
    let key = DeviceKey::new("xiaomi", "group-1");
    let (registry, _receivers) = stage_configuration(
        &store.devices(),
        &scope,
        &[Composition {
            key: key.clone(),
            label: "Kitchen".into(),
            members: vec![member.device.key.clone()],
        }],
        &[],
        &[],
    )
    .unwrap();
    let confirmed = store
        .devices()
        .apply_pending_configuration(&registry, |_| Ok((Vec::new(), Vec::new())))
        .unwrap()
        .unwrap();
    (key, confirmed.groups[0].endpoint)
}

#[test]
fn group_identity_lookup_resolves_the_reserved_endpoint_and_saved_label() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (key, endpoint) = compose(&store);
    let identity = store.devices().load_group_identity(&key).unwrap().unwrap();
    assert_eq!(identity.key, key);
    assert_eq!(identity.endpoint, endpoint);
    assert_eq!(identity.label, "Kitchen");
    assert_eq!(identity.unique_id.len(), 32);
    assert!(
        store
            .devices()
            .load_group_identity(&DeviceKey::new("xiaomi", "absent"))
            .unwrap()
            .is_none()
    );

    store
        .devices()
        .set_group_label(&key, "Renamed Kitchen")
        .unwrap();
    let renamed = store.devices().load_group_identity(&key).unwrap().unwrap();
    assert_eq!(renamed.label, "Renamed Kitchen");
    assert_eq!(renamed.unique_id, identity.unique_id);
    assert_eq!(renamed.endpoint, identity.endpoint);
}

#[test]
fn group_identity_lookup_rejects_inconsistent_storage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (key, _endpoint) = compose(&store);
    {
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute(
                "DELETE FROM endpoint_allocation
                 WHERE source=?1 AND device_id=?2 AND slot='group'",
                [&key.source, &key.id],
            )
            .unwrap();
    }
    assert!(store.devices().load_group_identity(&key).is_err());
}

#[test]
fn group_label_lookup_rejects_null_and_malformed_labels() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (key, _endpoint) = compose(&store);
    assert_eq!(
        store.devices().load_group_label(&key).unwrap().as_deref(),
        Some("Kitchen")
    );
    assert!(
        store
            .devices()
            .load_group_label(&DeviceKey::new("xiaomi", "absent"))
            .unwrap()
            .is_none()
    );

    assert!(
        store
            .connection()
            .execute(
                "UPDATE group_identity SET label=NULL WHERE source=?1 AND device_id=?2",
                [&key.source, &key.id],
            )
            .is_err()
    );
    assert_eq!(
        store.devices().load_group_label(&key).unwrap().as_deref(),
        Some("Kitchen")
    );

    {
        let connection = rusqlite::Connection::open(store.path()).unwrap();
        connection
            .execute(
                "UPDATE group_identity SET label=?3 WHERE source=?1 AND device_id=?2",
                rusqlite::params![key.source, key.id, "x".repeat(33)],
            )
            .unwrap();
    }
    let error = store.devices().load_group_label(&key).unwrap_err();
    assert!(error.to_string().contains("32"), "{error}");
}

#[test]
fn apply_rejects_orphaned_group_reservations_before_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (key, endpoint) = compose(&store);
    store
        .connection()
        .execute("DELETE FROM group_identity", [])
        .unwrap();
    let original = std::fs::read(store.path()).unwrap();
    let (registry, _receivers) = stage_configuration(
        &store.devices(),
        &CachedScope {
            uid: "uid".into(),
            home_id: "local".into(),
            group_id: group_id("uid", "local"),
        },
        &[Composition {
            key: key.clone(),
            label: "Kitchen".into(),
            members: vec![DeviceKey::new("xiaomi", "member-one")],
        }],
        &[],
        &[],
    )
    .unwrap();
    let error = store
        .devices()
        .apply_pending_configuration(&registry, |_| {
            panic!("an inconsistent reservation must be rejected before protocol mapping")
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Group endpoint reservation has no identity")
    );
    assert_eq!(std::fs::read(store.path()).unwrap(), original);
    assert_eq!(
        store.connection().query_row(
            "SELECT endpoint FROM endpoint_allocation WHERE source=?1 AND device_id=?2 AND slot='group'",
            [&key.source, &key.id],
            |row| row.get::<_, u16>(0),
        ).unwrap(),
        endpoint
    );
    assert!(store.devices().load_group_identity(&key).unwrap().is_none());
    assert!(registry.snapshot().pending.is_some());
}
