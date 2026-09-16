use migate::{
    device::{
        AccountId, DeviceDid, FeatureIdentity, FeatureRole, HomeId, PhysicalDeviceId, Property,
        PropertyValue, StateReport, StateSource,
    },
    storage::{
        PersistedState, PublishedFeatureDefinition, PublishedTopologyDelta, Store, TokenSet,
        XiaomiRecord,
    },
};
use rusqlite::Connection;
use tempfile::tempdir;

fn credentials(uid: &str, token: &str) -> XiaomiRecord {
    XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        tokens: TokenSet {
            access_token: token.into(),
            refresh_token: format!("refresh-{token}"),
            expires_at: 10_000,
            refresh_at: 5_000,
        },
        virtual_did: "123456789012345".into(),
        private_key_pem: "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----".into(),
        certificate_pem: "-----BEGIN CERTIFICATE-----\ncert\n-----END CERTIFICATE-----".into(),
    }
}

fn feature() -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("10001").unwrap(),
            home: HomeId::new("home-a").unwrap(),
            parent_did: DeviceDid::new("12345").unwrap(),
        },
        service_instance: 2,
        role: FeatureRole::Light,
    }
}

#[test]
fn auth_session_generation_changes_for_login_and_logout_but_not_refresh() {
    let directory = tempdir().unwrap();
    let first = Store::open(directory.path()).unwrap();
    let second = Store::open(directory.path()).unwrap();
    first
        .xiaomi()
        .replace(&credentials("10001", "token-a"))
        .unwrap();
    let login = second.xiaomi().snapshot().unwrap();
    assert!(login.session_generation.get() > 0);

    let mut refreshed = login.record.clone().unwrap().tokens;
    refreshed.access_token = "token-b".into();
    refreshed.refresh_token = "refresh-b".into();
    first.xiaomi().update_tokens(&refreshed).unwrap();
    assert_eq!(
        second.xiaomi().snapshot().unwrap().session_generation,
        login.session_generation
    );

    first.xiaomi().logout().unwrap();
    first
        .xiaomi()
        .replace(&credentials("10001", "token-c"))
        .unwrap();
    let relogin = second.xiaomi().snapshot().unwrap();
    assert!(relogin.session_generation > login.session_generation);
    assert_eq!(relogin.record.unwrap().uid, "10001");
}

#[test]
fn allocator_rejects_reserved_endpoint_without_publishing_identity() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let path = store.path().to_owned();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE feature_allocator SET next_endpoint=65535 WHERE id=1",
            [],
        )
        .unwrap();
    drop(connection);
    let store = Store::open(directory.path()).unwrap();
    assert!(store.devices().allocate_feature(&feature()).is_err());
    assert!(store.devices().load_features(false).unwrap().is_empty());
}

#[test]
fn persisted_state_rejects_wrong_variants_nonfinite_values_and_extra_rgb_components() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.devices().allocate_feature(&feature()).unwrap();
    let state = |property, value| PersistedState {
        feature: feature(),
        property,
        value,
        source: StateSource::Gateway,
        observed_at: 1,
        report_version: 1,
    };
    assert!(
        store
            .devices()
            .save_states(&[state(Property::Power, PropertyValue::Temperature(20.0),)])
            .is_err()
    );
    assert!(
        store
            .devices()
            .save_states(&[state(
                Property::Temperature,
                PropertyValue::Temperature(f64::NAN),
            )])
            .is_err()
    );
    let path = store.path().to_owned();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection.execute(
        "INSERT INTO feature_states(account_uid,home_id,parent_did,service_instance,role,property,value,source,observed_at,report_version)
         VALUES('10001','home-a','12345',2,'light','color',
                '{\"type\":\"color\",\"value\":{\"red\":1,\"green\":2,\"blue\":3,\"alpha\":4}}',
                'gateway',1,1)",
        [],
    ).unwrap();
    drop(connection);
    let store = Store::open(directory.path()).unwrap();
    assert!(store.devices().load_states().is_err());
}

#[test]
fn restored_report_versions_advance_the_shared_monotonic_counter() {
    let service = migate::device::DeviceService::new();
    let capabilities = migate::device::FeatureCapabilities::default();
    service.restore(feature(), "Light", capabilities);
    service.restore_last_known(StateReport::new(
        feature(),
        500,
        StateSource::Cache,
        1,
        [(Property::Power, PropertyValue::Power(true))],
    ));
    assert!(service.begin_query(&feature(), Property::Power) > 500);
    assert!(service.next_report_version() > 501);
}

#[test]
fn published_definition_and_identity_commit_atomically_and_restore_full_spec() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&credentials("10001", "token-a"))
        .unwrap();
    let session_generation = store.xiaomi().snapshot().unwrap().session_generation;
    let definition = PublishedFeatureDefinition {
        feature: feature(),
        model: "yeelink.light.ml9".into(),
        spec_document: include_str!("fixtures/miot_specs/yeelink.light.ml9.json").into(),
        name: "Desk light".into(),
    };
    let allocated = store
        .devices()
        .publish_topology(&PublishedTopologyDelta {
            account: AccountId::new("10001").unwrap(),
            session_generation,
            binding: None,
            devices: vec![],
            definitions: vec![definition.clone()],
            deactivate: vec![],
        })
        .unwrap()
        .unwrap()
        .remove(0);
    assert_eq!(allocated.endpoint, 2);
    assert_eq!(
        store
            .devices()
            .load_active_published_feature_definitions()
            .unwrap(),
        vec![definition]
    );

    let failed = PublishedFeatureDefinition {
        feature: FeatureIdentity {
            service_instance: 3,
            ..feature()
        },
        model: "yeelink.light.ml9".into(),
        spec_document: include_str!("fixtures/miot_specs/yeelink.light.ml9.json").into(),
        name: "Rejected".into(),
    };
    let path = store.path().to_owned();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_published_definition
             BEFORE INSERT ON published_feature_definitions
             WHEN NEW.service_instance=3
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    drop(connection);
    let store = Store::open(directory.path()).unwrap();
    assert!(
        store
            .devices()
            .publish_topology(&PublishedTopologyDelta {
                account: AccountId::new("10001").unwrap(),
                session_generation,
                binding: None,
                devices: vec![],
                definitions: vec![failed],
                deactivate: vec![],
            })
            .is_err()
    );
    assert_eq!(store.devices().load_features(false).unwrap().len(), 1);
}

#[test]
fn complete_topology_rolls_back_all_allocations_when_second_definition_fails() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&credentials("10001", "token-a"))
        .unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let connection = Connection::open(store.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_second_topology_feature BEFORE INSERT ON published_feature_definitions WHEN NEW.service_instance=3 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    drop(connection);
    let document = include_str!("fixtures/miot_specs/yeelink.light.ml9.json").to_owned();
    let first = PublishedFeatureDefinition {
        feature: feature(),
        model: "yeelink.light.ml9".into(),
        spec_document: document.clone(),
        name: "One".into(),
    };
    let second = PublishedFeatureDefinition {
        feature: FeatureIdentity {
            service_instance: 3,
            ..feature()
        },
        model: "yeelink.light.ml9".into(),
        spec_document: document,
        name: "Two".into(),
    };
    let delta = PublishedTopologyDelta {
        account: AccountId::new("10001").unwrap(),
        session_generation: session,
        binding: None,
        devices: vec![],
        definitions: vec![first, second],
        deactivate: vec![],
    };
    assert!(store.devices().publish_topology(&delta).is_err());
    assert!(store.devices().load_features(false).unwrap().is_empty());
    assert!(
        store
            .devices()
            .load_active_published_feature_definitions()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn active_allocation_without_archive_is_reported_as_corruption() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.devices().allocate_feature(&feature()).unwrap();
    assert!(
        store
            .devices()
            .load_active_published_feature_definitions()
            .is_err()
    );
}

#[test]
fn topology_transaction_does_not_replace_an_existing_same_account_home() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store
        .xiaomi()
        .replace(&credentials("10001", "token-a"))
        .unwrap();
    let session_generation = store.xiaomi().snapshot().unwrap().session_generation;
    let account = AccountId::new("10001").unwrap();
    let first = migate::storage::HomeBinding {
        account: account.clone(),
        home: HomeId::new("home-a").unwrap(),
        display_name: "First".into(),
    };
    assert!(
        store
            .devices()
            .publish_topology(&PublishedTopologyDelta {
                account: account.clone(),
                session_generation,
                binding: Some(first.clone()),
                devices: vec![],
                definitions: vec![],
                deactivate: vec![],
            })
            .unwrap()
            .is_some()
    );
    let replacement = migate::storage::HomeBinding {
        account: account.clone(),
        home: HomeId::new("home-b").unwrap(),
        display_name: "Second".into(),
    };
    let result = store
        .devices()
        .publish_topology(&PublishedTopologyDelta {
            account,
            session_generation,
            binding: Some(replacement),
            devices: vec![],
            definitions: vec![],
            deactivate: vec![],
        })
        .unwrap();

    assert!(result.is_none());
    assert_eq!(store.devices().load_binding().unwrap(), Some(first));
}
