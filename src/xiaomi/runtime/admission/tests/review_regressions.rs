use super::*;

fn setup() -> (
    tempfile::TempDir,
    Store,
    DeviceService,
    AdmissionController,
    AdmissionCatalog,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    store.xiaomi().replace(&credentials()).unwrap();
    let session = store.xiaomi().snapshot().unwrap().session_generation;
    let service = DeviceService::new();
    let commands = CommandRuntime::new(service.clone(), Rc::new(NoTransport));
    let admission = AdmissionController::new(
        store.devices(),
        service.clone(),
        commands,
        NetworkEpoch::new(7),
        session,
    );
    (directory, store, service, admission, catalog(session))
}

fn admit(admission: &mut AdmissionController, directory: &AdmissionCatalog) {
    admission
        .observe_gateway(
            proof(
                "0011223344556677",
                1,
                Some(true),
                Some(true),
                directory.session_generation,
            ),
            directory,
        )
        .unwrap();
}

fn enable_lan(
    admission: &mut AdmissionController,
    directory: &mut AdmissionCatalog,
    supported: bool,
) {
    directory.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    directory.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    let mut evidence = lan_proof(
        directory,
        directory.session_generation,
        0,
        NetworkEpoch::new(7),
    );
    evidence.evidence.native_supported = supported;
    assert!(admission.confirm_lan(&evidence, directory).unwrap());
}

#[test]
fn unchanged_catalog_preserves_verified_lan_path() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    admit(&mut admission, &catalog);
    enable_lan(&mut admission, &mut catalog, true);
    admission.apply_complete_catalog(&catalog).unwrap();
    assert!(admission.snapshot().unwrap().features[0].paths.lan);
}

#[test]
fn gateway_loss_does_not_turn_signed_identity_into_operation_support() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    admit(&mut admission, &catalog);
    enable_lan(&mut admission, &mut catalog, false);
    assert!(!admission.snapshot().unwrap().features[0].paths.lan);
    admission.remove_gateway(1).unwrap();
    assert!(!admission.snapshot().unwrap().features[0].paths.lan);
}

#[test]
fn retry_after_removal_write_failure_deactivates_persisted_endpoint() {
    let (_directory, store, service, mut admission, mut catalog) = setup();
    admit(&mut admission, &catalog);
    let removed_device = catalog.catalog.devices[0].clone();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_deactivation BEFORE UPDATE ON feature_identities
                 WHEN NEW.active=0 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    catalog.catalog.devices.clear();
    assert!(admission.apply_complete_catalog(&catalog).is_err());
    assert!(service.features().is_empty());
    connection
        .execute_batch("DROP TRIGGER fail_deactivation")
        .unwrap();
    admission.apply_complete_catalog(&catalog).unwrap();
    assert!(store.devices().load_features(true).unwrap().is_empty());
    catalog.catalog.devices.push(removed_device);
    admission.apply_complete_catalog(&catalog).unwrap();
    assert!(service.features().is_empty());
    admission
        .observe_gateway(
            proof(
                "0011223344556677",
                1,
                Some(true),
                Some(true),
                catalog.session_generation,
            ),
            &catalog,
        )
        .unwrap();
    assert!(!service.features().is_empty());
}

#[test]
fn cloud_recovery_cannot_reenable_paths_during_home_conflict() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    admit(&mut admission, &catalog);
    catalog.catalog.homes.push(CatalogHome {
        id: "home-b".into(),
        name: "Other home".into(),
        group_id: "8899aabbccddeeff".into(),
        rooms: vec![],
    });
    admission
        .observe_gateway(
            proof(
                "8899aabbccddeeff",
                2,
                Some(true),
                Some(true),
                catalog.session_generation,
            ),
            &catalog,
        )
        .unwrap();
    admission
        .observe_cloud(&CloudEvidence {
            account: catalog.account.clone(),
            session_generation: catalog.session_generation,
            status: CloudStatus::Ready,
        })
        .unwrap();
    assert_eq!(admission.status(), AdmissionStatus::SuspendedConflict);
    let snapshot = admission.snapshot().unwrap();
    assert!(
        snapshot
            .features
            .iter()
            .all(|feature| feature.paths == OperationPaths::default())
    );
    assert!(snapshot.features.iter().all(|feature| {
        feature
            .gateways
            .iter()
            .all(|gateway| !gateway.access && !gateway.push)
    }));
}

#[test]
fn rotated_token_discards_old_lan_session_evidence() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    admit(&mut admission, &catalog);
    enable_lan(&mut admission, &mut catalog, true);
    catalog.catalog.devices[0].token = Some(DeviceToken(vec![8; 16]));
    admission.apply_complete_catalog(&catalog).unwrap();
    let snapshot = admission.snapshot().unwrap();
    assert!(!snapshot.features[0].paths.lan);
    assert!(snapshot.features[0].lan_evidence.is_none());
}

#[test]
fn unchanged_channel_label_does_not_replace_authority_generation() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    catalog.catalog.devices[0].features[0].definition.name = "Desk channel".into();
    admit(&mut admission, &catalog);
    let previous = admission.snapshot().unwrap().features[0]
        .runtime
        .authority_generation;
    admission.apply_complete_catalog(&catalog).unwrap();
    assert_eq!(
        admission.snapshot().unwrap().features[0]
            .runtime
            .authority_generation,
        previous
    );
}

#[test]
fn changing_one_descriptor_preserves_other_device_authority() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    let document =
        include_str!("../../../../../tests/fixtures/miot_specs/cuco.plug.v3.json").to_owned();
    let compiled = compile_spec("cuco.plug.v3", &document).unwrap();
    let mut other = catalog.catalog.devices[0].clone();
    other.parent_did = "67890".into();
    other.model = "cuco.plug.v3".into();
    other.spec_type = Some(compiled.type_urn.clone());
    other.features = compiled.features;
    catalog.specifications.insert(compiled.type_urn, document);
    catalog.catalog.devices.push(other);
    let mut gateway = proof(
        "0011223344556677",
        1,
        Some(true),
        Some(true),
        catalog.session_generation,
    );
    let mut other_gateway_device = gateway.evidence.devices[0].clone();
    other_gateway_device.did = "67890".into();
    other_gateway_device.model = "cuco.plug.v3".into();
    gateway.evidence.devices.push(other_gateway_device);
    admission.observe_gateway(gateway, &catalog).unwrap();
    let previous = admission
        .snapshot()
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.identity.physical.parent_did.as_str() == "67890")
        .unwrap()
        .runtime;
    let type_urn = catalog.catalog.devices[0].spec_type.clone().unwrap();
    let document = catalog.specifications[&type_urn]
        .replace("\"value-range\":[1,100,1]", "\"value-range\":[1,90,1]");
    catalog.catalog.devices[0].features = compile_spec("yeelink.light.ml9", &document)
        .unwrap()
        .features;
    catalog.specifications.insert(type_urn, document);
    admission.apply_complete_catalog(&catalog).unwrap();
    let current = admission
        .snapshot()
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.identity == previous.identity)
        .unwrap()
        .runtime;
    assert_eq!(current.authority_generation, previous.authority_generation);
}

#[test]
fn only_verified_mcn02_legacy_read_enables_legacy_lan_operations() {
    let (_directory, _store, _service, mut admission, mut catalog) = setup();
    let document = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"
    ))
    .to_owned();
    let compiled = compile_spec("lumi.acpartner.mcn02", &document).unwrap();
    catalog.catalog.devices[0].model = "lumi.acpartner.mcn02".into();
    catalog.catalog.devices[0].spec_type = Some(compiled.type_urn.clone());
    catalog.catalog.devices[0].features = compiled.features;
    catalog.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    catalog.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    catalog.specifications.clear();
    catalog.specifications.insert(compiled.type_urn, document);
    admit(&mut admission, &catalog);
    let mut legacy = lan_proof(
        &catalog,
        catalog.session_generation,
        0,
        NetworkEpoch::new(7),
    );
    legacy.evidence.native_supported = false;
    legacy.legacy_operation = LegacyOperationEvidence::SuccessfulRead;
    assert!(admission.confirm_lan(&legacy, &catalog).unwrap());
    assert!(admission.snapshot().unwrap().features[0].paths.lan);

    let (_directory, _store, _service, mut other, mut ordinary) = setup();
    admit(&mut other, &ordinary);
    ordinary.catalog.devices[0].token = Some(DeviceToken(vec![7; 16]));
    ordinary.catalog.devices[0].local_ip = Some("192.168.1.9".into());
    let mut claimed_legacy = lan_proof(
        &ordinary,
        ordinary.session_generation,
        0,
        NetworkEpoch::new(7),
    );
    claimed_legacy.evidence.native_supported = false;
    claimed_legacy.legacy_operation = LegacyOperationEvidence::SuccessfulRead;
    assert!(other.confirm_lan(&claimed_legacy, &ordinary).unwrap());
    assert!(!other.snapshot().unwrap().features[0].paths.lan);
}
