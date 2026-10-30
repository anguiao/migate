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
