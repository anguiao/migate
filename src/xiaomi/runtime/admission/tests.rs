use super::*;
use crate::{
    storage::{DeviceToken, Store, TokenSet, XiaomiRecord},
    xiaomi::{
        catalog::{CatalogHome, compile_spec},
        discovery::{GatewayEndpoint, InterfaceRecord, LinkType},
        gateway::GatewayDevice,
        runtime::{CommandTransport, ControlPath, SendGuard, TransportCommand, TransportFailure},
    },
};
use futures_util::{FutureExt, future::LocalBoxFuture};
use std::{net::Ipv4Addr, rc::Rc, time::Duration};

struct NoTransport;
impl CommandTransport for NoTransport {
    fn send(
        &self,
        _: ControlPath,
        _: TransportCommand,
        _: Duration,
        _: SendGuard,
    ) -> LocalBoxFuture<'static, Result<(), TransportFailure>> {
        async { Err(TransportFailure::Unavailable) }.boxed_local()
    }
}

fn credentials() -> XiaomiRecord {
    credentials_for_uid("10001")
}

fn credentials_for_uid(uid: &str) -> XiaomiRecord {
    XiaomiRecord {
        uid: uid.into(),
        region: "cn".into(),
        oauth_client_uuid: "123e4567-e89b-12d3-a456-426614174000".into(),
        redirect_uri: "http://127.0.0.1/callback".into(),
        tokens: TokenSet {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 10_000,
            refresh_at: 5_000,
        },
        virtual_did: "123456789012345".into(),
        private_key_pem: "-----BEGIN PRIVATE KEY-----\nk\n-----END PRIVATE KEY-----".into(),
        certificate_pem: "-----BEGIN CERTIFICATE-----\nc\n-----END CERTIFICATE-----".into(),
    }
}

fn proof(
    group: &str,
    gateway_did: u64,
    access: Option<bool>,
    notify: Option<bool>,
    session_generation: AuthSessionGeneration,
) -> AuthenticatedGateway {
    let source = Ipv4Addr::new(192, 168, 1, 2);
    let target = Ipv4Addr::new(192, 168, 1, 3);
    let network = NetworkSnapshot::select(
        vec![InterfaceRecord {
            index: 4,
            name: "en0".into(),
            address: source,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            up: true,
            point_to_point: false,
            loopback: false,
            link_type: LinkType::Ethernet,
            physical: true,
        }],
        None,
    )
    .unwrap();
    let selected_endpoint = GatewayEndpoint {
        interface_index: 4,
        source_address: source,
        address: target,
        port: 8883,
    };
    AuthenticatedGateway {
        account: AccountId::new("10001").unwrap(),
        session_generation,
        candidate: GatewayCandidate {
            gateway_did,
            home_group: group.into(),
            endpoints: vec![selected_endpoint.clone()],
            unverified: true,
        },
        selected_endpoint,
        network,
        evidence: GatewayEvidence {
            gateway_did,
            peer_did: gateway_did.to_string(),
            epoch: NetworkEpoch::new(7),
            devices: vec![GatewayDevice {
                did: "12345".into(),
                name: "Light".into(),
                urn: "urn".into(),
                model: "yeelink.light.ml9".into(),
                online: None,
                spec_v2_access: access,
                push_available: notify,
            }],
        },
    }
}

fn catalog(session_generation: AuthSessionGeneration) -> AdmissionCatalog {
    let document =
        include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.ml9.json").to_owned();
    let compiled = compile_spec("yeelink.light.ml9", &document).unwrap();
    AdmissionCatalog {
        account: AccountId::new("10001").unwrap(),
        session_generation,
        catalog: DeviceCatalog {
            uid: "10001".into(),
            homes: vec![CatalogHome {
                id: "home-a".into(),
                name: "Home".into(),
                group_id: "0011223344556677".into(),
                rooms: vec![],
            }],
            devices: vec![CatalogDevice {
                home_id: "home-a".into(),
                room_id: None,
                parent_did: "12345".into(),
                name: "Light".into(),
                model: "yeelink.light.ml9".into(),
                spec_type: Some(compiled.type_urn.clone()),
                pid: Some(0),
                token: None,
                online: None,
                local_ip: None,
                parent_id: None,
                features: compiled.features,
            }],
        },
        specifications: HashMap::from([(compiled.type_urn, document)]),
    }
}

fn lan_proof(
    directory: &AdmissionCatalog,
    session_generation: AuthSessionGeneration,
    device_index: usize,
    epoch: NetworkEpoch,
) -> AuthenticatedLan {
    let interface = crate::xiaomi::discovery::NetworkInterface {
        index: 4,
        name: "en0".into(),
        address: Ipv4Addr::new(192, 168, 1, 2),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        prefix_len: 24,
    };
    let target = LanTarget::from_catalog(
        &directory.catalog.devices[device_index],
        "home-a",
        interface.clone(),
        epoch,
    )
    .unwrap();
    let network = NetworkSnapshot::select(
        vec![InterfaceRecord {
            index: interface.index(),
            name: "en0".into(),
            address: interface.address(),
            netmask: interface.netmask(),
            up: true,
            point_to_point: false,
            loopback: false,
            link_type: LinkType::Ethernet,
            physical: true,
        }],
        None,
    )
    .unwrap();
    AuthenticatedLan {
        account: directory.account.clone(),
        session_generation,
        evidence: LanEvidence {
            did: target.did(),
            address: target.address(),
            interface_index: interface.index(),
            epoch,
            signed_timestamp: 42,
            native_supported: true,
        },
        target,
        network,
        legacy_operation: LegacyOperationEvidence::Unverified,
    }
}

#[test]
fn authenticated_notify_only_device_binds_and_publishes_without_control_path() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let mut admission = AdmissionController::new(
        store.devices(),
        service,
        commands,
        NetworkEpoch::new(7),
        session,
    );
    assert_eq!(
        admission
            .observe_gateway(
                proof("0011223344556677", 1, None, Some(true), session),
                &catalog(session),
            )
            .unwrap(),
        AdmissionStatus::Active
    );
    let snapshot = admission.snapshot().unwrap();
    assert_eq!(snapshot.binding.unwrap().home.as_str(), "home-a");
    assert!(!snapshot.features.is_empty());
    assert!(!snapshot.features[0].paths.gateway);
    assert_eq!(
        snapshot.features[0].gateways,
        vec![GatewayPathEvidence {
            gateway_did: 1,
            access: false,
            push: true,
            online: None,
        }]
    );
}

#[test]
fn second_authenticated_home_suspends_existing_admission() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.homes.push(CatalogHome {
        id: "home-b".into(),
        name: "Other".into(),
        group_id: "8899aabbccddeeff".into(),
        rooms: vec![],
    });
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    assert_eq!(
        admission
            .observe_gateway(
                proof("8899aabbccddeeff", 2, Some(true), None, session),
                &directory,
            )
            .unwrap(),
        AdmissionStatus::SuspendedConflict
    );
    assert!(service.features().iter().all(|feature| !feature.admitted));
}

#[test]
fn same_uid_relogin_prevents_stale_gateway_from_republishing() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let stale_gateway = proof("0011223344556677", 1, Some(true), None, session);
    let mut stale_catalog = catalog(session);
    stale_catalog.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    stale_catalog.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    admission
        .observe_gateway(stale_gateway.clone(), &stale_catalog)
        .unwrap();
    let stale_lan = lan_proof(&stale_catalog, session, 0, NetworkEpoch::new(7));
    store.xiaomi().logout().unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    admission
        .observe_auth(&store.xiaomi().snapshot().unwrap())
        .unwrap();
    assert_eq!(
        admission
            .observe_gateway(stale_gateway, &stale_catalog)
            .unwrap(),
        AdmissionStatus::Unbound
    );
    assert!(!admission.confirm_lan(&stale_lan, &stale_catalog).unwrap());
    assert_eq!(service.features().len(), 1);
    assert!(service.features().iter().all(|feature| !feature.admitted));
}

#[test]
fn removing_conflicting_home_restores_remaining_fresh_gateway() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.homes.push(CatalogHome {
        id: "home-b".into(),
        name: "Other".into(),
        group_id: "8899aabbccddeeff".into(),
        rooms: vec![],
    });
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    admission
        .observe_gateway(
            proof("8899aabbccddeeff", 2, Some(true), None, session),
            &directory,
        )
        .unwrap();
    admission.remove_gateway(2).unwrap();

    assert_eq!(admission.status(), AdmissionStatus::Active);
    assert!(service.features().iter().all(|feature| feature.admitted));
    assert!(admission.snapshot().unwrap().features[0].paths.gateway);
}

#[test]
fn push_only_group_does_not_create_topology() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.devices[0].model = "mijia.light.group3".into();
    admission
        .observe_gateway(
            proof("0011223344556677", 1, None, Some(true), session),
            &directory,
        )
        .unwrap();

    assert!(service.features().is_empty());
    assert!(store.devices().load_features(true).unwrap().is_empty());
}

#[test]
fn topology_failure_does_not_publish_partial_memory() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    let mut second = directory.catalog.devices[0].features[0].clone();
    second.definition.service_instance += 100;
    let failed_instance = second.definition.service_instance;
    directory.catalog.devices[0].features.push(second);
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER fail_admission_feature
                 BEFORE INSERT ON published_feature_definitions
                 WHEN NEW.service_instance={failed_instance}
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
        ))
        .unwrap();
    drop(connection);

    assert!(
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &directory,
            )
            .is_err()
    );
    assert!(service.features().is_empty());
    assert!(admission.snapshot().unwrap().features.is_empty());
    assert!(store.devices().load_features(false).unwrap().is_empty());
}

#[test]
fn gateway_offline_does_not_override_catalog_cloud_reachability() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service,
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.devices[0].online = Some(true);
    let mut gateway = proof("0011223344556677", 1, Some(true), None, session);
    gateway.evidence.devices[0].online = Some(false);
    admission.observe_gateway(gateway, &directory).unwrap();
    admission
        .observe_cloud(&CloudEvidence {
            account: AccountId::new("10001").unwrap(),
            session_generation: session,
            status: CloudStatus::Ready,
        })
        .unwrap();
    let feature = &admission.snapshot().unwrap().features[0];
    assert!(!feature.paths.gateway);
    assert!(feature.paths.cloud);
}

#[test]
fn logout_preserves_endpoints_unavailable_and_same_uid_relogin_reuses_them() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &catalog(session),
        )
        .unwrap();
    let identity = store.devices().load_features(true).unwrap()[0].clone();

    store.xiaomi().logout().unwrap();
    admission
        .observe_auth(&store.xiaomi().snapshot().unwrap())
        .unwrap();
    assert_eq!(service.features().len(), 1);
    assert!(service.features().iter().all(|feature| !feature.admitted));
    assert!(
        admission
            .snapshot()
            .unwrap()
            .features
            .iter()
            .all(|feature| feature.paths == OperationPaths::default())
    );

    store.xiaomi().replace(&credentials()).unwrap();
    let relogin = store.xiaomi().snapshot().unwrap();
    admission.observe_auth(&relogin).unwrap();
    admission
        .observe_gateway(
            proof(
                "0011223344556677",
                1,
                Some(true),
                None,
                relogin.session_generation,
            ),
            &catalog(relogin.session_generation),
        )
        .unwrap();
    assert_eq!(store.devices().load_features(true).unwrap()[0], identity);
}

#[test]
fn complete_catalog_removes_device_after_last_gateway_without_treating_path_loss_as_removal() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let directory = catalog(session);
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    admission.remove_gateway(1).unwrap();
    assert_eq!(service.features().len(), 1);
    admission.apply_complete_catalog(&directory).unwrap();
    assert_eq!(service.features().len(), 1);

    let mut removed = directory;
    removed.catalog.devices.clear();
    admission.apply_complete_catalog(&removed).unwrap();
    assert!(service.features().is_empty());
    assert!(store.devices().load_features(true).unwrap().is_empty());
}

#[test]
fn existing_binding_accepts_fresh_lan_proof_without_gateway() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut initial = AdmissionController::new(
        store.devices(),
        service,
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    initial
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    drop(initial);

    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut restored = AdmissionController::new(
        store.devices(),
        service,
        commands,
        NetworkEpoch::new(7),
        session,
    );
    restored.restore_archived().unwrap();
    assert!(
        restored
            .confirm_lan(
                &lan_proof(&directory, session, 0, NetworkEpoch::new(7)),
                &directory,
            )
            .unwrap()
    );
    let snapshot = restored.snapshot().unwrap();
    assert!(snapshot.features[0].paths.lan);
    assert!(!snapshot.features[0].paths.gateway);
}

#[test]
fn missing_spec_keeps_archive_unavailable_but_complete_new_spec_deactivates_removed_feature() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let directory = catalog(session);
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();

    let mut missing = directory.clone();
    missing.specifications.clear();
    admission.apply_complete_catalog(&missing).unwrap();
    assert_eq!(service.features().len(), 1);
    assert!(!service.features()[0].admitted);
    assert_eq!(store.devices().load_features(true).unwrap().len(), 1);

    let mut removed = directory;
    removed.catalog.devices[0].features.clear();
    admission.apply_complete_catalog(&removed).unwrap();
    assert!(service.features().is_empty());
    assert!(store.devices().load_features(true).unwrap().is_empty());
}

#[test]
fn different_uid_login_removes_old_account_from_memory() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &catalog(session),
        )
        .unwrap();
    store
        .xiaomi()
        .replace(&credentials_for_uid("20002"))
        .unwrap();
    admission
        .observe_auth(&store.xiaomi().snapshot().unwrap())
        .unwrap();
    assert!(service.features().is_empty());
    assert!(store.devices().load_features(true).unwrap().is_empty());

    let replacement = store.xiaomi().snapshot().unwrap();
    let mut new_catalog = catalog(replacement.session_generation);
    new_catalog.account = AccountId::new("20002").unwrap();
    new_catalog.catalog.uid = "20002".into();
    let mut new_gateway = proof(
        "0011223344556677",
        1,
        Some(true),
        None,
        replacement.session_generation,
    );
    new_gateway.account = AccountId::new("20002").unwrap();
    assert_eq!(
        admission
            .observe_gateway(new_gateway, &new_catalog)
            .unwrap(),
        AdmissionStatus::Active
    );
    assert!(service.features().iter().all(|feature| feature.admitted));
}

#[test]
fn rename_updates_metadata_without_reallocating_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let directory = catalog(session);
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    let allocated = store.devices().load_features(true).unwrap()[0].clone();
    let mut renamed = directory;
    renamed.catalog.devices[0].name = "Desk lamp".into();
    admission.apply_complete_catalog(&renamed).unwrap();

    assert_eq!(service.features()[0].name, "Desk lamp");
    assert_eq!(store.devices().load_features(true).unwrap()[0], allocated);
}

#[test]
fn network_change_keeps_historical_admission_but_does_not_admit_a_new_catalog_device() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let mut directory = catalog(session);
    directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    let mut second = directory.catalog.devices[0].clone();
    second.parent_did = "67890".into();
    second.name = "Other light".into();
    second.token = Some(DeviceToken(vec![8; 16]));
    second.local_ip = Some("192.168.1.10".into());
    directory.catalog.devices.push(second);
    let mut gateway = proof("0011223344556677", 1, Some(true), None, session);
    let mut second_gateway_device = gateway.evidence.devices[0].clone();
    second_gateway_device.did = "67890".into();
    gateway.evidence.devices.push(second_gateway_device);
    admission.observe_gateway(gateway, &directory).unwrap();
    admission
        .observe_cloud(&CloudEvidence {
            account: AccountId::new("10001").unwrap(),
            session_generation: session,
            status: CloudStatus::Ready,
        })
        .unwrap();
    assert_eq!(service.features().len(), 2);

    admission.invalidate(NetworkEpoch::new(8));
    let mut new_device = directory.catalog.devices[0].clone();
    new_device.parent_did = "new-cloud-only".into();
    new_device.name = "New cloud-only light".into();
    new_device.token = None;
    new_device.local_ip = None;
    directory.catalog.devices.push(new_device);
    admission.apply_complete_catalog(&directory).unwrap();
    let features = service.features();
    assert_eq!(
        features.iter().filter(|feature| feature.admitted).count(),
        2
    );
    assert!(
        features
            .iter()
            .all(|feature| { feature.identity.physical.parent_did.as_str() != "new-cloud-only" })
    );
    let snapshot = admission.snapshot().unwrap();
    assert_eq!(snapshot.epoch, NetworkEpoch::new(8));
    assert!(snapshot.features.iter().all(|feature| feature.paths
        == OperationPaths {
            cloud: true,
            ..OperationPaths::default()
        }));
}

#[test]
fn complete_catalog_restores_persisted_admission_after_restart_for_the_bound_account_home() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let catalog = catalog(session);
    {
        let service = DeviceService::new();
        let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
        let mut admission = AdmissionController::new(
            store.devices(),
            service,
            commands,
            NetworkEpoch::new(7),
            session,
        );
        admission
            .observe_gateway(
                proof("0011223344556677", 1, Some(true), None, session),
                &catalog,
            )
            .unwrap();
    }

    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut restored = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(9),
        session,
    );
    restored.restore_archived().unwrap();
    restored.apply_complete_catalog(&catalog).unwrap();

    assert_eq!(restored.status(), AdmissionStatus::Active);
    assert_eq!(service.features().len(), 1);
    assert!(service.features()[0].admitted);
    assert_eq!(
        service.features()[0].identity.physical.home.as_str(),
        "home-a"
    );
}

#[test]
fn descriptor_change_revokes_before_storage_and_publishes_a_new_generation_after_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let mut admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    let directory = catalog(session);
    admission
        .observe_gateway(
            proof("0011223344556677", 1, Some(true), None, session),
            &directory,
        )
        .unwrap();
    let previous = admission.snapshot().unwrap().features[0].runtime.clone();
    let mut changed = directory;
    let type_urn = changed.catalog.devices[0].spec_type.clone().unwrap();
    let document = changed.specifications[&type_urn]
        .replace("\"value-range\":[1,100,1]", "\"value-range\":[1,90,1]");
    changed.catalog.devices[0].features = compile_spec("yeelink.light.ml9", &document)
        .unwrap()
        .features;
    changed.specifications.insert(type_urn, document);
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_descriptor_update
                 BEFORE UPDATE ON published_feature_definitions
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    drop(connection);

    assert!(admission.apply_complete_catalog(&changed).is_err());
    assert!(service.features().iter().all(|feature| !feature.admitted));
    assert!(admission.snapshot().unwrap().features.is_empty());
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection
        .execute_batch("DROP TRIGGER fail_descriptor_update")
        .unwrap();
    drop(connection);
    admission.apply_complete_catalog(&changed).unwrap();
    let current = admission.snapshot().unwrap().features[0].runtime.clone();
    assert_ne!(current.descriptor, previous.descriptor);
    assert!(current.authority_generation > previous.authority_generation);
}

mod review_regressions;
