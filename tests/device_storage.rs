use migate::{
    device::{
        AccountId, CurtainMovement, DeviceService, FeatureCapabilities,
        FeatureIdentity as DeviceFeatureIdentity, FeatureRole, HomeId, HvacMode, Percent,
        PhysicalDeviceId, PresenceState, Property, PropertyState, PropertyValue, RgbColor,
        StateReport, StateSource, SwingMode, VacuumCleanMode, VacuumOperationalState,
    },
    storage::{
        CachedSpec, CatalogDeviceMetadata, CatalogHomeRecord, CatalogRoomRecord, CatalogSnapshot,
        DeviceRecord, HomeBinding, PersistedState, PublishedFeatureDefinition,
        PublishedTopologyDelta, Store, TokenSet, XiaomiRecord,
    },
};
use rusqlite::Connection;
use std::error::Error as _;

fn credentials(uid: &str) -> XiaomiRecord {
    XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        oauth_client_uuid: "550e8400-e29b-41d4-a716-446655440000".into(),
        redirect_uri: "http://homeassistant.local/callback".into(),
        tokens: TokenSet {
            access_token: format!("access-{uid}"),
            refresh_token: format!("refresh-{uid}"),
            expires_at: 20,
            refresh_at: 10,
        },
        virtual_did: "1".into(),
        private_key_pem: "private".into(),
        certificate_pem: "certificate".into(),
    }
}

fn catalog_snapshot() -> CatalogSnapshot {
    CatalogSnapshot {
        account: AccountId::new("uid-1").unwrap(),
        homes: vec![CatalogHomeRecord {
            home_id: "home-1".into(),
            name: "Home".into(),
            group_id: "0123456789abcdef".into(),
        }],
        rooms: vec![CatalogRoomRecord {
            home_id: "home-1".into(),
            room_id: "room-1".into(),
            name: "Room".into(),
        }],
        devices: vec![(
            DeviceRecord {
                identity: physical(),
                model: "xiaomi.switch.w3".into(),
                name: "Switch".into(),
                room_id: Some("room-1".into()),
                admitted: false,
            },
            CatalogDeviceMetadata {
                spec_type: Some("urn:miot-spec-v2:device:switch:0000:test:1".into()),
                pid: Some(8),
                local_ip: Some("192.168.1.2".into()),
                parent_id: Some("gateway".into()),
                online: None,
                feature_document: "[{\"siid\":2,\"role\":\"load\",\"name\":\"Left\"}]".into(),
            },
            Some([7; 16]),
        )],
        specs: vec![CachedSpec {
            type_urn: "urn:miot-spec-v2:device:switch:0000:test:1".into(),
            document: "{\"type\":\"urn:miot-spec-v2:device:switch:0000:test:1\",\"services\":[]}"
                .into(),
            fetched_at: 10,
        }],
    }
}

fn physical() -> PhysicalDeviceId {
    PhysicalDeviceId {
        account: AccountId::new("uid-1").unwrap(),
        home: HomeId::new("home-1").unwrap(),
        parent_did: "did-1".parse().unwrap(),
    }
}

#[test]
fn complete_catalog_snapshot_is_atomic_and_guarded_by_auth_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    let snapshot = catalog_snapshot();
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&snapshot, revision)
            .unwrap()
    );
    assert_eq!(
        store
            .devices()
            .load_catalog(&AccountId::new("uid-1").unwrap())
            .unwrap()
            .devices[0]
            .1
            .feature_document,
        snapshot.devices[0].1.feature_document
    );
    let mut duplicate_home = snapshot.clone();
    duplicate_home.homes.push(CatalogHomeRecord {
        home_id: "home-1".into(),
        name: "Duplicate".into(),
        group_id: "fedcba9876543210".into(),
    });
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&duplicate_home, revision)
            .is_err()
    );
    assert_eq!(
        store
            .devices()
            .load_catalog(&AccountId::new("uid-1").unwrap())
            .unwrap()
            .devices
            .len(),
        1
    );

    let mut mixed_account = snapshot.clone();
    mixed_account.devices[0].0.identity.account = AccountId::new("uid-other").unwrap();
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&mixed_account, revision)
            .is_err()
    );
    assert_eq!(store.devices().load_devices().unwrap().len(), 1);

    let mut unnamed_room = snapshot.clone();
    unnamed_room.rooms[0].name.clear();
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&unnamed_room, revision)
            .unwrap()
    );
    assert_eq!(
        store
            .devices()
            .load_catalog(&AccountId::new("uid-1").unwrap())
            .unwrap()
            .rooms[0]
            .name,
        ""
    );
    store.devices().allocate_feature(&feature()).unwrap();
    let empty = CatalogSnapshot {
        account: AccountId::new("uid-1").unwrap(),
        homes: snapshot.homes.clone(),
        rooms: vec![],
        devices: vec![],
        specs: vec![],
    };
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&empty, revision)
            .unwrap()
    );
    assert!(store.devices().load_features(true).unwrap().is_empty());
    assert_eq!(store.devices().load_token(&physical()).unwrap(), None);
    store.xiaomi().logout().unwrap();
    assert!(
        !store
            .devices()
            .replace_catalog_if_revision(&snapshot, revision)
            .unwrap()
    );
    assert_eq!(store.devices().load_token(&physical()).unwrap(), None);
}

fn feature() -> DeviceFeatureIdentity {
    DeviceFeatureIdentity {
        physical: physical(),
        service_instance: 3,
        role: FeatureRole::Load,
    }
}

#[test]
fn binding_catalog_tokens_and_caches_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let devices = store.devices();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let auth = store.xiaomi().snapshot().unwrap();
    let snapshot = catalog_snapshot();
    assert!(
        devices
            .replace_catalog_if_revision(&snapshot, auth.revision)
            .unwrap()
    );
    let binding = HomeBinding {
        account: AccountId::new("uid-1").unwrap(),
        home: HomeId::new("home-1").unwrap(),
        display_name: "My home".into(),
    };
    assert!(
        devices
            .publish_topology(&PublishedTopologyDelta {
                account: AccountId::new("uid-1").unwrap(),
                session_generation: auth.session_generation,
                binding: Some(binding.clone()),
                devices: vec![DeviceRecord {
                    identity: physical(),
                    model: "xiaomi.switch.w3".into(),
                    name: "Wall switch".into(),
                    room_id: Some("room-1".into()),
                    admitted: true,
                }],
                definitions: vec![],
                deactivate: vec![],
            })
            .unwrap()
            .is_some()
    );
    assert_eq!(devices.load_binding().unwrap(), Some(binding));
    assert_eq!(devices.load_devices().unwrap().len(), 1);
    assert_eq!(devices.load_token(&physical()).unwrap(), Some(vec![7; 16]));
    assert!(
        devices
            .load_spec("urn:miot-spec-v2:device:switch:0000:test:1")
            .unwrap()
            .is_some()
    );
}

#[test]
fn foreign_keys_reject_orphan_writes_and_orphaned_databases() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(
        store
            .devices()
            .save_states(&[PersistedState {
                feature: feature(),
                property: Property::Power,
                value: PropertyValue::Power(true),
                source: StateSource::Cloud,
                observed_at: 1,
                report_version: 1,
            }])
            .is_err()
    );
    let path = store.path().to_owned();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .unwrap();
    connection
        .execute(
            r#"INSERT INTO feature_states(
            account_uid,home_id,parent_did,service_instance,role,property,value,
            source,observed_at,report_version
         ) VALUES('uid-1','home-1','missing',1,'load','power',
                  '{"type":"power","value":true}','cloud',1,1)"#,
            [],
        )
        .unwrap();
    drop(connection);
    assert!(Store::open(dir.path()).is_err());
}

#[test]
fn cache_documents_accept_json_whitespace_and_reject_invalid_or_corrupt_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    let spec = CachedSpec {
        type_urn: "urn:test".into(),
        document: "{\n  \"services\": []\n}\n".into(),
        fetched_at: 5,
    };
    let mut snapshot = catalog_snapshot();
    snapshot.specs = vec![spec.clone()];
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&snapshot, revision)
            .unwrap()
    );
    assert_eq!(store.devices().load_spec("urn:test").unwrap(), Some(spec));
    let invalid = CachedSpec {
        type_urn: "urn:invalid".into(),
        document: "{invalid secret}".into(),
        fetched_at: 6,
    };
    snapshot.specs = vec![invalid];
    let error = store
        .devices()
        .replace_catalog_if_revision(&snapshot, revision)
        .unwrap_err();
    assert_eq!(error.path(), store.path());
    assert!(!format!("{error:?} {error}").contains("invalid secret"));
    Connection::open(store.path())
        .unwrap()
        .execute(
            "INSERT INTO miot_specs(type_urn,document,fetched_at) VALUES('corrupt','not-json',1)",
            [],
        )
        .unwrap();
    assert!(store.devices().load_spec("corrupt").is_err());
}

#[test]
fn feature_identity_allocation_is_stable_monotonic_and_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let session_generation = store.xiaomi().snapshot().unwrap().session_generation;
    let devices = store.devices();
    let first = devices.allocate_feature(&feature()).unwrap();
    assert_eq!(first.endpoint, 2);
    assert_eq!(devices.allocate_feature(&feature()).unwrap(), first);
    assert!(
        devices
            .publish_topology(&PublishedTopologyDelta {
                account: AccountId::new("uid-1").unwrap(),
                session_generation,
                binding: None,
                devices: vec![],
                definitions: vec![],
                deactivate: vec![feature()],
            })
            .unwrap()
            .is_some()
    );

    let mut second_feature = feature();
    second_feature.service_instance = 4;
    let second = devices.allocate_feature(&second_feature).unwrap();
    assert_eq!(second.endpoint, 3);
    assert_ne!(second.public_id, first.public_id);

    assert!(
        devices
            .publish_topology(&PublishedTopologyDelta {
                account: AccountId::new("uid-1").unwrap(),
                session_generation,
                binding: None,
                devices: vec![],
                definitions: vec![PublishedFeatureDefinition {
                    feature: feature(),
                    model: "yeelink.light.ml9".into(),
                    spec_document: include_str!("fixtures/miot_specs/yeelink.light.ml9.json")
                        .into(),
                    name: "Load".into(),
                }],
                deactivate: vec![],
            })
            .unwrap()
            .is_some()
    );
    assert_eq!(devices.allocate_feature(&feature()).unwrap(), first);
}

#[test]
fn held_storage_lock_does_not_publish_or_consume_a_feature_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let locker = Connection::open(store.path()).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    // Keep the lock held until allocation returns, regardless of thread scheduling.
    let result = store.devices().allocate_feature(&feature());
    locker.execute_batch("ROLLBACK").unwrap();
    let error = result.unwrap_err();
    assert!(matches!(
        error.source().unwrap().downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(error, _))
            if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ));
    assert!(store.devices().load_features(false).unwrap().is_empty());

    let identity = store.devices().allocate_feature(&feature()).unwrap();
    assert_eq!(identity.endpoint, 2);
    assert_eq!(
        store.devices().allocate_feature(&feature()).unwrap(),
        identity
    );
    assert_eq!(store.devices().load_features(false).unwrap(), [identity]);
}

#[test]
fn persisted_current_state_restarts_as_last_known() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let identity = store.devices().allocate_feature(&feature()).unwrap();
    store
        .devices()
        .save_states(&[PersistedState {
            feature: identity.feature.clone(),
            property: Property::Power,
            value: PropertyValue::Power(true),
            source: StateSource::Lan,
            observed_at: 42,
            report_version: 8,
        }])
        .unwrap();
    drop(store);

    let restored = Store::open(dir.path())
        .unwrap()
        .devices()
        .load_states()
        .unwrap();
    assert_eq!(
        restored,
        vec![PersistedState {
            feature: feature(),
            property: Property::Power,
            value: PropertyValue::Power(true),
            source: StateSource::Lan,
            observed_at: 42,
            report_version: 8,
        }]
    );
    let service = DeviceService::new();
    service.restore(feature(), "Load", FeatureCapabilities::default());
    for state in restored {
        service.restore_last_known(StateReport::new(
            state.feature,
            state.report_version,
            state.source,
            state.observed_at,
            [(state.property, state.value)],
        ));
    }
    assert!(matches!(
        service
            .snapshot(&feature())
            .unwrap()
            .property(Property::Power),
        Some(PropertyState::LastKnown {
            value: PropertyValue::Power(true),
            ..
        })
    ));
}

#[test]
fn every_property_value_round_trips_and_invalid_values_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.devices().allocate_feature(&feature()).unwrap();
    let values = vec![
        (Property::Power, PropertyValue::Power(true)),
        (
            Property::Brightness,
            PropertyValue::Percent(Percent::new(42.5).unwrap()),
        ),
        (
            Property::ColorTemperature,
            PropertyValue::ColorTemperature(4000),
        ),
        (
            Property::Color,
            PropertyValue::Color(RgbColor {
                red: 1,
                green: 2,
                blue: 3,
            }),
        ),
        (Property::Temperature, PropertyValue::Temperature(-12.25)),
        (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Dry)),
        (Property::FanSpeed, PropertyValue::FanSpeed(7)),
        (
            Property::SwingMode,
            PropertyValue::SwingMode(SwingMode::Horizontal),
        ),
        (
            Property::CurtainMovement,
            PropertyValue::CurtainMovement(CurtainMovement::Closing),
        ),
        (Property::Oscillation, PropertyValue::Oscillation(true)),
        (Property::Illuminance, PropertyValue::Illuminance(123.75)),
        (Property::Motion, PropertyValue::Motion(true)),
        (
            Property::Occupancy,
            PropertyValue::Occupancy(PresenceState::Unknown),
        ),
        (Property::Contact, PropertyValue::ContactOpen(false)),
        (
            Property::VacuumCleanMode,
            PropertyValue::VacuumCleanMode(VacuumCleanMode::VacuumAndMop),
        ),
        (
            Property::VacuumOperationalState,
            PropertyValue::VacuumOperationalState(VacuumOperationalState::Returning),
        ),
        (
            Property::VacuumFault,
            PropertyValue::VacuumFault("brush-blocked".into()),
        ),
    ];
    let states = values
        .iter()
        .enumerate()
        .map(|(index, (property, value))| PersistedState {
            feature: feature(),
            property: *property,
            value: value.clone(),
            source: StateSource::Lan,
            observed_at: index as i64,
            report_version: index as u64 + 1,
        })
        .collect::<Vec<_>>();
    store.devices().save_states(&states).unwrap();
    let loaded = store.devices().load_states().unwrap();
    for (property, value) in values {
        assert_eq!(
            loaded
                .iter()
                .find(|state| state.property == property)
                .map(|state| &state.value),
            Some(&value)
        );
    }

    assert!(
        store
            .devices()
            .save_states(&[PersistedState {
                feature: feature(),
                property: Property::Temperature,
                value: PropertyValue::Temperature(f64::INFINITY),
                source: StateSource::Cloud,
                observed_at: 1,
                report_version: 100,
            }])
            .is_err()
    );
    Connection::open(store.path())
        .unwrap()
        .execute(
            "UPDATE feature_states SET value='not-json' WHERE property='power'",
            [],
        )
        .unwrap();
    assert!(store.devices().load_states().is_err());
}

#[test]
fn logout_clears_device_tokens_but_preserves_topology_and_nonsensitive_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    let mut snapshot = catalog_snapshot();
    snapshot.devices[0].2 = Some([9; 16]);
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&snapshot, revision)
            .unwrap()
    );
    let allocated = store.devices().allocate_feature(&feature()).unwrap();

    store.xiaomi().logout().unwrap();

    assert_eq!(store.devices().load_token(&physical()).unwrap(), None);
    assert_eq!(store.devices().load_devices().unwrap().len(), 1);
    assert_eq!(
        store.devices().allocate_feature(&feature()).unwrap(),
        allocated
    );
}

#[test]
fn stale_auth_revision_cannot_restore_a_device_token_after_logout() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    Store::open(dir.path()).unwrap().xiaomi().logout().unwrap();
    assert!(
        !store
            .devices()
            .replace_catalog_if_revision(&catalog_snapshot(), revision)
            .unwrap()
    );
    assert_eq!(store.devices().load_token(&physical()).unwrap(), None);
    assert!(store.devices().load_devices().unwrap().is_empty());
}

#[test]
fn replacing_account_deactivates_old_account_topology() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.xiaomi().replace(&credentials("uid-1")).unwrap();
    let revision = store.xiaomi().snapshot().unwrap().revision;
    assert!(
        store
            .devices()
            .replace_catalog_if_revision(&catalog_snapshot(), revision)
            .unwrap()
    );
    store.devices().allocate_feature(&feature()).unwrap();
    store.xiaomi().replace(&credentials("uid-2")).unwrap();
    assert!(store.devices().load_features(true).unwrap().is_empty());
    assert!(!store.devices().load_devices().unwrap()[0].admitted);
}
