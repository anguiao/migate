use super::configuration::{corrupt, fixture, load, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, PhysicalDevice, Power},
    xiaomi::capability::tests::source::SourceFixture,
};

#[test]
fn standard_air_conditioner_round_trips_identity_bindings_and_unknown_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "ac",
        "generic.ac.model",
        include_str!("../../../tests/fixtures/miot/lumi-mcn04.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    drop(store);
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&store);
    assert_eq!(restored, planned);
    let entry = &restored.devices[0];
    assert_ne!(entry.endpoint, entry.functions[0].endpoint);
    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![SourceFixture::committed(&restored, &entry.key.id)],
    );
    assert!(!unchanged.changed && !unchanged.rebuild);
    let native = entry.native.as_ref().unwrap().device().functions()[0]
        .air_conditioner()
        .unwrap();
    assert_eq!(native.speed().codes(), [0, 1, 2, 3]);
    let (physical, _) = PhysicalDevice::new(
        restored.derive().served[&DeviceKey::new("xiaomi", "ac")]
            .description
            .clone(),
    );
    let snapshot = physical.functions()[0].snapshot();
    assert_eq!(snapshot.power(), Some(Power::Unknown));
    let ac = snapshot.air_conditioner().unwrap();
    assert_eq!(
        (
            ac.mode.value,
            ac.temperature.value,
            ac.speed.value,
            ac.swing.value
        ),
        (None, None, None, None)
    );
    let valid: serde_json::Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, replacement) in [
        (
            "/devices/0/native/functions/0/mode/codes",
            serde_json::json!([0, 0, 2, 3, 4]),
        ),
        (
            "/devices/0/native/functions/0/speed/codes",
            serde_json::json!([0, 1, 2]),
        ),
        (
            "/devices/0/native/functions/0/temperature/step",
            serde_json::json!(0),
        ),
        (
            "/devices/0/native/functions/0/swing/siid",
            serde_json::json!(3),
        ),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = replacement;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
}

#[test]
fn mcn04_patch_round_trips_bindings_without_restoring_observations() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "ac",
        "lumi.acpartner.mcn04",
        include_str!("../../../tests/fixtures/miot/lumi-mcn04.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let restored = load(&store);
    assert_eq!(restored, planned);
    let entry = &restored.devices[0];
    let ac = entry.native.as_ref().unwrap().device().functions()[0]
        .air_conditioner()
        .unwrap();
    assert_eq!(
        ac.observation_bindings(),
        vec![
            crate::xiaomi::capability::PropertyBinding { siid: 8, piid: 7 },
            crate::xiaomi::capability::PropertyBinding { siid: 8, piid: 6 }
        ]
    );
    assert_eq!(ac.temperature().encode(26), serde_json::json!(26));
    let (physical, _) = PhysicalDevice::new(
        restored.derive().served[&DeviceKey::new("xiaomi", "ac")]
            .description
            .clone(),
    );
    assert_eq!(
        physical.functions()[0]
            .snapshot()
            .air_conditioner()
            .unwrap(),
        Default::default()
    );
}

#[test]
fn mcn02_profile_is_reconstructed_from_an_existing_configuration_without_persistence() {
    use rs_matter::persist::{FABRIC_KEYS_START, PERSISTENT_SUBSCRIPTIONS_START};

    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "ac",
        "lumi.acpartner.mcn02",
        include_str!("../../../tests/fixtures/miot/lumi-mcn02-synthetic.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let stored = raw(&store);
    assert!(!stored.contains("lumi.mcn02.legacy-miio"));
    let mut legacy: serde_json::Value = serde_json::from_str(&stored).unwrap();
    let wire = &mut legacy["devices"][0]["native"];
    assert!(wire.get("legacy_profile").is_none());
    wire["recognition"]["core"] = serde_json::json!([]);
    corrupt(&store, &legacy);
    let before = raw(&store);
    store
        .matter()
        .put(FABRIC_KEYS_START + 1, b"fabric")
        .unwrap();
    store
        .matter()
        .put(PERSISTENT_SUBSCRIPTIONS_START, b"subscription")
        .unwrap();
    let restored = load(&store);
    assert_eq!(raw(&store), before);
    assert_eq!(restored.devices[0].unique_id, planned.devices[0].unique_id);
    assert_eq!(restored.devices[0].endpoint, planned.devices[0].endpoint);
    assert_eq!(restored.devices[0].functions, planned.devices[0].functions);
    assert_eq!(
        restored.devices[0]
            .native
            .as_ref()
            .unwrap()
            .legacy_profile()
            .unwrap()
            .patch()
            .target(),
        "air-conditioner:2"
    );

    let refreshed = next(
        &restored,
        scope("account", "home"),
        vec![fixture(
            "ac",
            "lumi.acpartner.mcn02",
            include_str!("../../../tests/fixtures/miot/lumi-mcn02-synthetic.json"),
        )],
    );
    assert!(refreshed.changed);
    assert!(!refreshed.rebuild);
    assert_eq!(
        refreshed.operations[0].kind,
        crate::configuration::OperationKind::Reused
    );
    store
        .devices()
        .commit_configuration_with_reset(
            &refreshed.configuration,
            &[],
            refreshed.rebuild.then(crate::matter::subscription_keys),
        )
        .unwrap();
    assert_eq!(
        store.matter().get(FABRIC_KEYS_START + 1).unwrap(),
        Some(b"fabric".to_vec())
    );
    assert_eq!(
        store.matter().get(PERSISTENT_SUBSCRIPTIONS_START).unwrap(),
        Some(b"subscription".to_vec())
    );

    legacy["devices"][0]["native"]
        .as_object_mut()
        .unwrap()
        .remove("recognition");
    corrupt(&store, &legacy);
    assert!(
        load(&store).devices[0]
            .native
            .as_ref()
            .unwrap()
            .legacy_profile()
            .is_some()
    );
}
