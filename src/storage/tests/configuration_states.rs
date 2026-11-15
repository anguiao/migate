//! Current state bindings persist atomically, independently of live observations.
use super::configuration::{fixture, load, next, raw, save, scope};
use crate::configuration::CommittedConfiguration;
use crate::xiaomi::capability::tests::source::SourceFixture;
use serde_json::json;

#[test]
fn restoring_state_devices_preserves_their_bindings_and_child_endpoints() {
    let samples = [
        (
            "isa.magnet.dw2hl",
            include_str!("../../../tests/fixtures/miot/isa-dw2hl.json"),
            "contact:2:2",
        ),
        (
            "linp.magnet.m1",
            include_str!("../../../tests/fixtures/miot/linp-m1.json"),
            "contact:2:1004",
        ),
        (
            "xiaomi.motion.pir1",
            include_str!("../../../tests/fixtures/miot/xiaomi-pir1.json"),
            "motion:2:1024",
        ),
        (
            "linp.sensor_occupy.hb01",
            include_str!("../../../tests/fixtures/miot/linp-hb01.json"),
            "occupancy:2:1",
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let devices = samples
        .iter()
        .map(|(model, text, _)| fixture(model, model, text))
        .collect();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        devices,
    )
    .configuration;
    save(&store, &planned);
    drop(store);

    let reopened = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&reopened);
    assert_eq!(restored, planned);
    for (model, _, expected) in samples {
        let key = crate::device::DeviceKey::new("xiaomi", model);
        let entry = restored.device(&key).unwrap();
        let function = entry
            .functions
            .iter()
            .find(|function| function.id == expected)
            .unwrap();
        assert_ne!(function.endpoint, entry.endpoint, "{model}");
    }
    let sources = restored
        .devices
        .iter()
        .map(|entry| SourceFixture::committed(&restored, &entry.key.id))
        .collect();
    let unchanged = next(&restored, scope("account", "home"), sources);
    assert!(!unchanged.changed);
    assert!(!unchanged.rebuild);
}

#[test]
fn adding_state_preserves_existing_child_endpoints() {
    let outlet = fixture(
        "mixed",
        "cuco.plug.v3",
        include_str!("../../../tests/fixtures/miot/outlet.json"),
    );
    let contact = fixture(
        "mixed",
        "isa.magnet.dw2hl",
        include_str!("../../../tests/fixtures/miot/isa-dw2hl.json"),
    );
    let occupancy = fixture(
        "mixed",
        "izq.sensor_occupy.trio",
        include_str!("../../../tests/fixtures/miot/izq-trio.json"),
    );
    let initial = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![outlet.clone()],
    )
    .configuration;
    let old = &initial.devices[0];
    assert_ne!(old.functions[0].endpoint, old.endpoint);
    let mixed = |extra: bool| {
        let mut instance = outlet.raw.instance.clone();
        let mut contact = contact.raw.instance["services"][1].clone();
        contact["iid"] = json!(49);
        instance["services"].as_array_mut().unwrap().push(contact);
        if extra {
            let mut occupancy = occupancy.raw.instance["services"][1].clone();
            occupancy["iid"] = json!(50);
            instance["services"].as_array_mut().unwrap().push(occupancy);
        }
        SourceFixture::new("mixed", outlet.model(), instance)
    };
    let plan = next(&initial, scope("account", "home"), vec![mixed(false)]);
    assert!(plan.rebuild);
    let converted = plan.configuration;
    let current = &converted.devices[0];
    assert_eq!(current.unique_id, old.unique_id);
    assert_eq!(current.endpoint, old.endpoint);
    assert_eq!(current.label, old.label);
    assert!(
        current
            .functions
            .iter()
            .all(|function| function.endpoint != current.endpoint)
    );
    assert_eq!(current.functions[0].endpoint, old.functions[0].endpoint);
    let plan = next(&converted, scope("account", "home"), vec![mixed(true)]);
    assert!(plan.rebuild);
    assert_eq!(
        &plan.configuration.devices[0].functions[..2],
        &current.functions[..]
    );
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    save(&store, &plan.configuration);
    drop(store);
    let reopened = crate::storage::Store::open(directory.path()).unwrap();
    assert_eq!(load(&reopened), plan.configuration);
}

#[test]
fn partial_motion_configuration_cannot_be_restored_or_published() {
    use super::configuration::corrupt;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "state",
        "xiaomi.motion.pir1",
        include_str!("../../../tests/fixtures/miot/xiaomi-pir1.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let valid: serde_json::Value = serde_json::from_str(&raw(&store)).unwrap();
    for field in ["event", "state", "arguments"] {
        let mut invalid = valid.clone();
        invalid["devices"][0]["native"]["functions"][1]
            .as_object_mut()
            .unwrap()
            .remove(field);
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{field}");
    }
    for (pointer, value) in [
        (
            "/devices/0/native/functions/1/event/siid",
            serde_json::json!(10),
        ),
        (
            "/devices/0/native/functions/1/event/eiid",
            serde_json::json!(0),
        ),
        (
            "/devices/0/native/functions/1/state/mapping/format",
            serde_json::json!("float"),
        ),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &invalid);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
    save(&store, &planned);
    assert_eq!(load(&store), planned);
}

#[test]
fn a_missing_enumeration_mapping_cannot_be_restored_as_native_unknown() {
    use super::configuration::corrupt;
    let directory = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let native = fixture(
        "state",
        "linp.sensor_occupy.hb01",
        include_str!("../../../tests/fixtures/miot/linp-hb01.json"),
    );
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let mut invalid: serde_json::Value = serde_json::from_str(&raw(&store)).unwrap();
    invalid["devices"][0]["native"]["functions"][1]["state"]["mapping"]["values"][2]
        .as_object_mut()
        .unwrap()
        .remove("active");
    corrupt(&store, &invalid);
    assert!(store.devices().load_configuration().is_err());
}
