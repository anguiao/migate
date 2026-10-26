//! Current state bindings persist atomically, independently of live observations.
use super::configuration::{fixture, load, next, raw, save, scope};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, PhysicalDevice, StateInput},
    xiaomi::capability::{NativeDescription, ParsedDevice},
};

#[test]
fn state_bindings_restore_without_runtime_observations_or_endpoint_changes() {
    for (model, text) in [
        (
            "isa.magnet.dw2hl",
            include_str!("../../../tests/fixtures/miot/isa-dw2hl.json"),
        ),
        (
            "linp.magnet.m1",
            include_str!("../../../tests/fixtures/miot/linp-m1.json"),
        ),
        (
            "xiaomi.motion.pir1",
            include_str!("../../../tests/fixtures/miot/xiaomi-pir1.json"),
        ),
        (
            "linp.sensor_occupy.hb01",
            include_str!("../../../tests/fixtures/miot/linp-hb01.json"),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(directory.path()).unwrap();
        let native = fixture("state", model, text);
        let planned = next(
            &CommittedConfiguration::empty(),
            scope("account", "home"),
            vec![native],
        )
        .configuration;
        save(&store, &planned);
        let key = DeviceKey::new("xiaomi", "state");
        let (device, _) = PhysicalDevice::new(planned.derive().served[&key].description.clone());
        for function in device
            .functions()
            .iter()
            .filter(|function| function.description().kind.is_state_sensor())
        {
            function
                .observe_state(
                    Some(true),
                    StateInput::Property(serde_json::json!(1)),
                    Some(10),
                    20,
                )
                .unwrap();
        }
        let encoded = raw(&store);
        assert!(!encoded.contains("source_time"));
        assert!(!encoded.contains("received_at"));
        drop(store);
        let reopened = crate::storage::Store::open(directory.path()).unwrap();
        let restored = load(&reopened);
        assert_eq!(restored, planned);
        let (device, receivers) =
            PhysicalDevice::new(restored.derive().served[&key].description.clone());
        assert!(receivers.is_empty());
        for function in device
            .functions()
            .iter()
            .filter(|function| function.description().kind.is_state_sensor())
        {
            assert_eq!(function.snapshot().state_sensor().unwrap().active, None);
        }
        let entry = &restored.devices[0];
        assert!(
            entry
                .functions
                .iter()
                .all(|function| function.endpoint != entry.endpoint)
        );
        let unchanged = next(
            &restored,
            scope("account", "home"),
            vec![entry.native.clone().unwrap()],
        );
        assert!(!unchanged.changed);
        assert!(!unchanged.rebuild);
    }
}

#[test]
fn adding_state_converts_single_control_then_preserves_existing_child_endpoints() {
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
    assert_eq!(old.functions[0].endpoint, old.endpoint);
    let mixed = |extra: bool| {
        let mut functions = outlet.device().functions().to_vec();
        functions.extend_from_slice(contact.device().functions());
        if extra {
            let original = occupancy
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == "occupancy")
                .unwrap();
            functions.push(crate::xiaomi::capability::ParsedFunction::occupancy(
                "occupancy",
                "Occupancy",
                crate::xiaomi::capability::StateBinding::new(
                    crate::xiaomi::capability::PropertyBinding { siid: 50, piid: 1 },
                    original.state().unwrap().mapping().clone(),
                )
                .unwrap(),
            ));
        }
        NativeDescription::new(
            outlet.model().into(),
            outlet.instance_type().into(),
            ParsedDevice::new(outlet.device().key().clone(), functions, None).unwrap(),
        )
        .unwrap()
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
    assert_ne!(current.functions[0].endpoint, old.functions[0].endpoint);
    let plan = next(&converted, scope("account", "home"), vec![mixed(true)]);
    assert!(plan.rebuild);
    assert_eq!(
        &plan.configuration.devices[0].functions[..2],
        &current.functions[..]
    );
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
