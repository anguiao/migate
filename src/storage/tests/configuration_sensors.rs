//! Sensor capability persistence in the current schema-9 private codec.

use super::configuration::{corrupt, fixture, load, next, raw, save, scope, seed};
use crate::{
    configuration::CommittedConfiguration,
    device::{DeviceKey, FunctionKind, MeasurementRange},
    xiaomi::capability::{MeasurementUnit, NumericFormat, PropertyBinding, parse_device_for_model},
};
use serde_json::{Value, json};

const SENSOR: &str = include_str!("../../../tests/fixtures/miot/miaomiaoce-t6.json");
const OUTLET: &str = include_str!("../../../tests/fixtures/miot/outlet.json");

fn sensor(id: &str) -> crate::xiaomi::capability::NativeDescription {
    crate::xiaomi::capability::NativeDescription::new(
        "miaomiaoce.sensor_ht.t6".into(),
        "urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:miaomiaoce-t6:1".into(),
        parse_device_for_model(
            DeviceKey::new("xiaomi", id),
            "miaomiaoce.sensor_ht.t6",
            &serde_json::from_str(SENSOR).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn recognition_metadata_survives_storage_and_metadata_only_changes_reuse_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let original = sensor("sensor");
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    save(&store, &planned);
    let loaded = load(&store);
    assert_eq!(
        loaded.devices[0]
            .native
            .as_ref()
            .unwrap()
            .device()
            .recognition(),
        original.device().recognition()
    );
    let mut instance: Value = serde_json::from_str(SENSOR).unwrap();
    instance["type"] = json!("urn:miot-spec-v2:device:air-conditioner:0000A004:example:1");
    let changed = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        instance["type"].as_str().unwrap().into(),
        parse_device_for_model(original.device().key().clone(), original.model(), &instance)
            .unwrap(),
    )
    .unwrap();
    let plan = next(&loaded, scope("account", "home"), vec![changed]);
    assert!(plan.changed);
    assert!(!plan.rebuild);
    assert_eq!(
        plan.operations[0].kind,
        crate::configuration::OperationKind::Reused
    );
    assert_eq!(
        plan.configuration.devices[0].functions,
        loaded.devices[0].functions
    );
}

#[test]
fn single_measurements_and_mixed_controls_round_trip_with_current_parent_layout() {
    for wanted in ["temperature:3:1001", "humidity:3:1008", "mixed"] {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(directory.path()).unwrap();
        let original = sensor("sensor");
        let functions = if wanted == "mixed" {
            let mut functions = fixture("sensor", "cuco.plug.v3", OUTLET)
                .device()
                .functions()
                .to_vec();
            functions.extend_from_slice(original.device().functions());
            functions
        } else {
            original
                .device()
                .functions()
                .iter()
                .filter(|function| function.id() == wanted)
                .cloned()
                .collect()
        };
        let native = crate::xiaomi::capability::NativeDescription::new(
            "sample".into(),
            if wanted == "mixed" {
                "urn:miot-spec-v2:device:outlet:0000A002:sample:1".into()
            } else {
                original.instance_type().into()
            },
            crate::xiaomi::capability::ParsedDevice::new_with_battery(
                original.device().key().clone(),
                functions,
                None,
                original.device().battery().cloned(),
            )
            .unwrap(),
        )
        .unwrap();
        let planned = next(
            &CommittedConfiguration::empty(),
            scope("account", "home"),
            vec![native],
        )
        .configuration;
        save(&store, &planned);
        let restored = load(&store);
        assert_eq!(planned, restored);
        assert!(
            restored.devices[0]
                .functions
                .iter()
                .all(|function| function.endpoint != restored.devices[0].endpoint)
        );
        assert!(
            restored.devices[0]
                .native
                .as_ref()
                .unwrap()
                .device()
                .battery()
                .is_some()
        );
        let unchanged = next(
            &restored,
            scope("account", "home"),
            vec![restored.devices[0].native.clone().unwrap()],
        );
        assert!(!unchanged.changed);
        assert!(!unchanged.rebuild);
    }
}

#[test]
fn sensor_bindings_round_trip_with_parent_and_child_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let original = sensor("sensor");
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    save(&store, &planned);

    let loaded = load(&store);
    assert_eq!(loaded, planned);
    let device = &loaded.devices[0];
    assert_eq!(device.key, DeviceKey::new("xiaomi", "sensor"));
    assert_eq!(device.endpoint, 2);
    assert_eq!(
        device
            .functions
            .iter()
            .map(|function| (function.id.as_str(), function.endpoint))
            .collect::<Vec<_>>(),
        [("temperature:3:1001", 3), ("humidity:3:1008", 4)]
    );
    let native = device.native.as_ref().unwrap();
    assert_eq!(native.model(), "miaomiaoce.sensor_ht.t6");
    assert!(native.device().is_read_only());
    assert_eq!(
        native.device().functions()[0].kind(),
        FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap())
    );
    assert_eq!(
        native.device().functions()[1].kind(),
        FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap())
    );
    assert_eq!(
        native.device().functions()[0]
            .measurement()
            .unwrap()
            .property(),
        PropertyBinding {
            siid: 3,
            piid: 1001
        }
    );

    let encoded: Value = serde_json::from_str(&raw(&store)).unwrap();
    let functions = &encoded["devices"][0]["native"]["functions"];
    assert_eq!(functions[0]["kind"], json!("temperature"));
    assert_eq!(functions[0]["measurement"]["format"], json!("float"));
    assert_eq!(functions[0]["measurement"]["unit"], json!("celsius"));
    assert_eq!(functions[0]["measurement"]["min"], json!(-30.0));
    assert_eq!(functions[0]["measurement"]["max"], json!(100.0));
    assert_eq!(functions[1]["kind"], json!("humidity"));
    assert_eq!(functions[1]["measurement"]["unit"], json!("percentage"));
    let battery = &encoded["devices"][0]["native"]["battery"];
    assert_eq!(battery["property"], json!({"siid": 2, "piid": 1003}));
    assert_eq!(battery["format"], json!("uint8"));
    assert_eq!(battery["unit"], json!("percentage"));
    assert_eq!(battery["min"], json!(0.0));
    assert_eq!(battery["max"], json!(100.0));
    for target in [
        &functions[0]["measurement"],
        &functions[1]["measurement"],
        battery,
    ] {
        for forbidden in ["step", "value", "source_time", "received_at", "reading"] {
            assert!(target.get(forbidden).is_none(), "{forbidden}");
        }
    }
}

#[test]
fn old_outlet_and_light_configurations_remain_readable_alongside_sensors() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let outlet = fixture("outlet", "cuco.plug.v3", OUTLET);
    let sensor = sensor("sensor");
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![outlet, sensor],
    )
    .configuration;
    save(&store, &planned);
    assert_eq!(load(&store), planned);
    let loaded = load(&store);
    let outlet = loaded
        .devices
        .iter()
        .find(|device| device.key.id == "outlet")
        .unwrap();
    assert_eq!(
        outlet.native.as_ref().unwrap().device().functions()[0].kind(),
        FunctionKind::Outlet
    );
    let sensor = loaded
        .devices
        .iter()
        .find(|device| device.key.id == "sensor")
        .unwrap();
    assert_eq!(sensor.functions.len(), 2);
    assert_eq!(outlet.endpoint, 2);
    assert_eq!(sensor.endpoint, 4);
    assert_eq!(
        sensor
            .functions
            .iter()
            .map(|f| f.endpoint)
            .collect::<Vec<_>>(),
        [5, 6]
    );
}

#[test]
fn multiple_sensors_keep_distinct_identities_names_and_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let mut planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![sensor("first"), sensor("second")],
    )
    .configuration;
    planned.devices[0].label = "First".into();
    planned.devices[1].label = "Second".into();
    save(&store, &planned);
    let loaded = load(&store);
    assert_eq!(loaded.devices[0].label, "First");
    assert_eq!(loaded.devices[1].label, "Second");
    assert_ne!(loaded.devices[0].unique_id, loaded.devices[1].unique_id);
    assert_ne!(loaded.devices[0].endpoint, loaded.devices[1].endpoint);
    assert_ne!(
        loaded.devices[0].functions[0].endpoint,
        loaded.devices[1].functions[0].endpoint
    );
    assert_eq!(
        loaded.devices[0].native.as_ref().unwrap().device().key().id,
        "first"
    );
    assert_eq!(
        loaded.devices[1].native.as_ref().unwrap().device().key().id,
        "second"
    );
}

#[test]
fn absent_optional_battery_field_remains_readable() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    seed(&store, vec![sensor("sensor")]);
    let mut old: Value = serde_json::from_str(&raw(&store)).unwrap();
    old["devices"][0]["native"]
        .as_object_mut()
        .unwrap()
        .remove("battery");
    corrupt(&store, &old);
    let loaded = load(&store);
    let native = loaded.devices[0].native.as_ref().unwrap();
    assert!(native.device().battery().is_none());
    assert_eq!(native.device().functions().len(), 2);
}

#[test]
fn optional_battery_bindings_round_trip_without_runtime_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let original = sensor("sensor");
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    save(&store, &planned);
    let loaded = load(&store);
    assert_eq!(loaded, planned);
    let battery = loaded.devices[0]
        .native
        .as_ref()
        .unwrap()
        .device()
        .battery()
        .unwrap();
    assert_eq!(
        battery.property(),
        PropertyBinding {
            siid: 2,
            piid: 1003
        }
    );
    assert_eq!(battery.format(), NumericFormat::Uint8);
    assert_eq!(battery.range().min(), 0.0);
    assert_eq!(battery.range().max(), 100.0);
    assert_eq!(loaded.devices[0].functions.len(), 2);
    assert_eq!(loaded.devices[0].endpoint, 2);
    assert_eq!(
        loaded.devices[0]
            .functions
            .iter()
            .map(|function| function.endpoint)
            .collect::<Vec<_>>(),
        [3, 4]
    );
}

#[test]
fn invalid_battery_storage_shapes_are_rejected_at_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    seed(&store, vec![sensor("sensor")]);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, value) in [
        ("/devices/0/native/battery/property/siid", json!(0)),
        ("/devices/0/native/battery/format", json!("bool")),
        ("/devices/0/native/battery/unit", json!("celsius")),
        ("/devices/0/native/battery/min", json!(-1.0)),
        ("/devices/0/native/battery/max", json!(101.0)),
        ("/devices/0/native/battery/max", json!(0.0)),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &changed);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
    let mut changed = valid.clone();
    changed["devices"][0]["native"]["battery"]["unknown"] = json!(true);
    corrupt(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
    let mut changed = valid;
    changed["devices"][0]["native"]["battery"]["property"] = json!({"siid": 3, "piid": 1001});
    corrupt(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn invalid_sensor_measurement_ranges_are_rejected_at_the_storage_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    seed(&store, vec![sensor("sensor")]);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    for (pointer, value) in [
        (
            "/devices/0/native/functions/0/measurement/min",
            json!(100.0),
        ),
        (
            "/devices/0/native/functions/0/measurement/max",
            json!(-30.0),
        ),
        (
            "/devices/0/native/functions/0/measurement/unit",
            json!("fahrenheit"),
        ),
        (
            "/devices/0/native/functions/0/measurement/format",
            json!("bool"),
        ),
    ] {
        let mut changed = valid.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        corrupt(&store, &changed);
        assert!(store.devices().load_configuration().is_err(), "{pointer}");
    }
    let mut changed = valid;
    changed["devices"][0]["native"]["functions"][0]["kind"] = json!("humidity");
    corrupt(&store, &changed);
    assert!(store.devices().load_configuration().is_err());
}

#[test]
fn native_measurement_metadata_is_preserved_without_a_battery_field() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let mut sensor = sensor("sensor");
    sensor = crate::xiaomi::capability::NativeDescription::new(
        sensor.model().into(),
        sensor.instance_type().into(),
        crate::xiaomi::capability::ParsedDevice::new(
            sensor.device().key().clone(),
            sensor.device().functions().to_vec(),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![sensor],
    )
    .configuration;
    save(&store, &planned);
    let loaded = load(&store);
    let measurement = loaded.devices[0]
        .native
        .as_ref()
        .unwrap()
        .device()
        .functions()[0]
        .measurement()
        .unwrap();
    assert_eq!(measurement.format(), NumericFormat::Float);
    assert_eq!(measurement.unit(), MeasurementUnit::Celsius);
    assert_eq!(measurement.range().min(), -30.0);
    assert_eq!(measurement.range().max(), 100.0);
}

#[test]
fn a_sensor_range_change_is_a_publish_shape_change_but_keeps_its_identity() {
    use crate::device::MeasurementRange;
    use crate::xiaomi::capability::{MeasurementBinding, ParsedDevice, ParsedFunction};

    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let original = sensor("sensor");
    let first = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    save(&store, &first);
    let old = first.devices[0].clone();
    assert_eq!(old.configuration_version, 1);
    // The first publication changes the empty root picture and bumps it once.
    assert_eq!(first.root_version, 2);

    let functions = original.device().functions().to_vec();
    let measurement = functions[0].measurement().unwrap();
    let changed_binding = MeasurementBinding::new(
        measurement.property(),
        measurement.format(),
        measurement.unit(),
        MeasurementRange::new(-20.0, 80.0).unwrap(),
    )
    .unwrap();
    let changed = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        ParsedDevice::new_with_battery(
            original.device().key().clone(),
            vec![
                ParsedFunction::temperature("temperature:3:1001", "Temperature", changed_binding),
                functions[1].clone(),
            ],
            None,
            original.device().battery().cloned(),
        )
        .unwrap(),
    )
    .unwrap();
    let plan = next(&first, scope("account", "home"), vec![changed]);
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations[0].kind,
        crate::configuration::OperationKind::ReplacedNative
    );
    let planned = plan.configuration;
    save(&store, &planned);
    let loaded = load(&store);
    assert_eq!(loaded.devices[0].key, old.key);
    assert_eq!(loaded.devices[0].unique_id, old.unique_id);
    assert_eq!(loaded.devices[0].endpoint, old.endpoint);
    assert_eq!(loaded.devices[0].functions, old.functions);
    assert_eq!(loaded.devices[0].configuration_version, 2);
    assert_eq!(loaded.root_version, 3);
    assert_eq!(
        loaded.devices[0]
            .native
            .as_ref()
            .unwrap()
            .device()
            .functions()[0]
            .measurement()
            .unwrap()
            .range()
            .max(),
        80.0
    );
}

#[test]
fn measurement_and_battery_step_changes_do_not_change_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let original = sensor("sensor");
    let first = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![original.clone()],
    )
    .configuration;
    save(&store, &first);
    let root = first.root_version;

    let mut instance: Value = serde_json::from_str(SENSOR).unwrap();
    for service in instance["services"].as_array_mut().unwrap() {
        for property in service["properties"].as_array_mut().unwrap() {
            if let Some(range) = property["value-range"].as_array_mut() {
                range[2] = json!(0.01);
            }
        }
    }
    let changed = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        parse_device_for_model(original.device().key().clone(), original.model(), &instance)
            .unwrap(),
    )
    .unwrap();
    let plan = next(&first, scope("account", "home"), vec![changed]);
    assert!(!plan.changed);
    assert!(!plan.rebuild);
    assert_eq!(
        plan.operations[0].kind,
        crate::configuration::OperationKind::Reused
    );
    assert_eq!(plan.configuration.root_version, root);
    assert_eq!(
        plan.configuration.devices[0].configuration_version,
        first.devices[0].configuration_version
    );
}

#[test]
fn adding_battery_changes_the_published_shape_not_the_sensor_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let with_battery = sensor("sensor");
    let without_battery = crate::xiaomi::capability::NativeDescription::new(
        with_battery.model().into(),
        with_battery.instance_type().into(),
        crate::xiaomi::capability::ParsedDevice::new(
            with_battery.device().key().clone(),
            with_battery.device().functions().to_vec(),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let first = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![without_battery],
    )
    .configuration;
    save(&store, &first);
    let old = first.devices[0].clone();
    let plan = next(&first, scope("account", "home"), vec![with_battery]);
    assert!(plan.rebuild);
    assert_eq!(
        plan.operations[0].kind,
        crate::configuration::OperationKind::ReplacedNative
    );
    assert_eq!(plan.configuration.devices[0].key, old.key);
    assert_eq!(plan.configuration.devices[0].unique_id, old.unique_id);
    assert_eq!(plan.configuration.devices[0].endpoint, old.endpoint);
    assert_eq!(plan.configuration.devices[0].functions, old.functions);
    assert_eq!(plan.configuration.devices[0].configuration_version, 2);
    save(&store, &plan.configuration);
    assert_eq!(load(&store), plan.configuration);
}

#[test]
fn specific_battery_parse_diagnostics_survive_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let description =
        crate::xiaomi::capability::tests::parse_device(DeviceKey::new("xiaomi", "sensor"), &{
            let mut value: Value = serde_json::from_str(SENSOR).unwrap();
            value["services"].as_array_mut().unwrap().retain(|service| {
                !service["type"]
                    .as_str()
                    .is_some_and(|urn| urn.contains("service:battery:"))
            });
            value
        })
        .unwrap();
    let native = crate::xiaomi::capability::NativeDescription::new(
        "miaomiaoce.sensor_ht.t6".into(),
        "urn:miot-spec-v2:device:temperature-humidity-sensor:0000A00A:miaomiaoce-t6:1".into(),
        description,
    )
    .unwrap();
    let expected = native.device().battery_issue().unwrap().to_owned();
    let diagnostic_only = crate::xiaomi::capability::NativeDescription::new(
        native.model().into(),
        native.instance_type().into(),
        crate::xiaomi::capability::ParsedDevice::new_with_battery_diagnostic(
            native.device().key().clone(),
            native.device().functions().to_vec(),
            None,
            None,
            Some("battery capability not declared".into()),
        )
        .unwrap(),
    )
    .unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let loaded = load(&store);
    assert_eq!(loaded, planned);
    let device = loaded.devices[0].native.as_ref().unwrap().device();
    assert!(device.battery().is_none());
    assert_eq!(device.battery_issue(), Some(expected.as_str()));
    let plan = next(&loaded, scope("account", "home"), vec![diagnostic_only]);
    assert!(plan.changed);
    assert!(!plan.rebuild);
    assert_eq!(
        plan.operations[0].kind,
        crate::configuration::OperationKind::Reused
    );
}

#[test]
fn absent_optional_battery_diagnostic_remains_absent() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    let instance: Value = serde_json::from_str(SENSOR).unwrap();
    let parsed = parse_device_for_model(
        DeviceKey::new("xiaomi", "sensor"),
        "miaomiaoce.sensor_ht.t6",
        &instance,
    )
    .unwrap();
    let device = crate::xiaomi::capability::ParsedDevice::new(
        parsed.key().clone(),
        parsed.functions().to_vec(),
        None,
    )
    .unwrap();
    let native = crate::xiaomi::capability::NativeDescription::new(
        "miaomiaoce.sensor_ht.t6".into(),
        instance["type"].as_str().unwrap().into(),
        device,
    )
    .unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let mut old: Value = serde_json::from_str(&raw(&store)).unwrap();
    old["devices"][0]["native"]
        .as_object_mut()
        .unwrap()
        .remove("battery_issue");
    corrupt(&store, &old);
    let loaded = load(&store);
    let device = loaded.devices[0].native.as_ref().unwrap().device();
    assert!(device.battery().is_none());
    assert_eq!(device.battery_issue(), None);
}

#[test]
fn co2_configuration_restores_identity_bindings_and_endpoints_without_live_readings() {
    let directory = tempfile::tempdir().unwrap();
    let original = fixture(
        "co2",
        "xiaomi.sensor_ht.new3pr",
        include_str!("../../../tests/fixtures/miot/xiaomi-new3pr.json"),
    );
    let native = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        crate::xiaomi::capability::ParsedDevice::new_with_battery(
            original.device().key().clone(),
            original
                .device()
                .functions()
                .iter()
                .filter(|function| function.id() == "co2:8:1029")
                .cloned()
                .collect(),
            None,
            original.device().battery().cloned(),
        )
        .unwrap(),
    )
    .unwrap();
    let store = crate::storage::Store::open(directory.path()).unwrap();
    let planned = next(
        &CommittedConfiguration::empty(),
        scope("account", "home"),
        vec![native],
    )
    .configuration;
    save(&store, &planned);
    let (physical, _) = crate::device::PhysicalDevice::new(
        planned.derive().served[&DeviceKey::new("xiaomi", "co2")]
            .description
            .clone(),
    );
    physical.functions()[0]
        .observe_measurement(700.0, Some(100), 200)
        .unwrap();
    let wire = raw(&store);
    assert!(!wire.contains("received_at"));
    assert!(!wire.contains("source_time"));
    drop(store);
    let reopened = crate::storage::Store::open(directory.path()).unwrap();
    let restored = load(&reopened);
    assert_eq!(restored, planned);
    let device = &restored.devices[0];
    assert_eq!(device.functions.len(), 1);
    assert_ne!(device.functions[0].endpoint, device.endpoint);
    let co2 = &device.native.as_ref().unwrap().device().functions()[0];
    assert_eq!(co2.id(), "co2:8:1029");
    assert_eq!(
        co2.kind(),
        FunctionKind::CarbonDioxide(MeasurementRange::new(400.0, 5000.0).unwrap())
    );
    assert_eq!(co2.measurement().unwrap().unit(), MeasurementUnit::Ppm);
    assert_eq!(
        co2.measurement().unwrap().property(),
        PropertyBinding {
            siid: 8,
            piid: 1029
        }
    );
    let (restarted, _) = crate::device::PhysicalDevice::new(
        restored.derive().served[&DeviceKey::new("xiaomi", "co2")]
            .description
            .clone(),
    );
    assert_eq!(
        restarted.functions()[0]
            .snapshot()
            .measurement()
            .unwrap()
            .value,
        None
    );
    let unchanged = next(
        &restored,
        scope("account", "home"),
        vec![device.native.clone().unwrap()],
    );
    assert!(!unchanged.changed);
    assert!(!unchanged.rebuild);
}

#[test]
fn illuminance_restores_current_identity_and_range_without_persisting_readings_or_steps() {
    for (model, text, battery) in [
        (
            "yeelink.light.nl1",
            include_str!("../../../tests/fixtures/miot/yeelink-nl1.json"),
            true,
        ),
        (
            "linp.sensor_occupy.hb01",
            include_str!("../../../tests/fixtures/miot/linp-hb01.json"),
            false,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(directory.path()).unwrap();
        let native = fixture("illuminance", model, text);
        let key = native.device().key().clone();
        let mut planned = next(
            &CommittedConfiguration::empty(),
            scope("account", "home"),
            vec![native.clone()],
        )
        .configuration;
        planned.devices[0].label = "Room light level".into();
        save(&store, &planned);
        let (physical, _) =
            crate::device::PhysicalDevice::new(planned.derive().served[&key].description.clone());
        physical.functions()[0]
            .observe_measurement(0.125, Some(100), 200)
            .unwrap();
        let encoded = raw(&store);
        assert!(!encoded.contains("source_time"));
        assert!(!encoded.contains("received_at"));
        assert!(!encoded.contains("step"));
        drop(store);
        let reopened = crate::storage::Store::open(directory.path()).unwrap();
        let restored = load(&reopened);
        assert_eq!(restored, planned);
        let description = &restored.derive().served[&key].description;
        assert_eq!(description.label(), "Room light level");
        assert_eq!(description.has_battery(), battery);
        let (restarted, _) = crate::device::PhysicalDevice::new(description.clone());
        assert_eq!(
            restarted.functions()[0]
                .snapshot()
                .measurement()
                .unwrap()
                .value,
            None
        );
        assert_ne!(
            restored.devices[0].endpoint,
            restored.devices[0].functions[0].endpoint
        );
        let mut instance: Value = serde_json::from_str(text).unwrap();
        for service in instance["services"].as_array_mut().unwrap() {
            if let Some(properties) = service["properties"].as_array_mut() {
                for property in properties {
                    if property["value-range"].is_array() {
                        property["value-range"][2] = json!(2);
                    }
                }
            }
        }
        let changed_step = super::configuration::native("illuminance", model, instance.clone());
        let unchanged = next(&restored, scope("account", "home"), vec![changed_step]);
        assert!(!unchanged.changed);
        assert!(!unchanged.rebuild);
        for service in instance["services"].as_array_mut().unwrap() {
            if let Some(properties) = service["properties"].as_array_mut() {
                for property in properties {
                    if property["unit"] == "lux" {
                        property["value-range"][1] = json!(5000);
                    }
                }
            }
        }
        let changed_range = super::configuration::native("illuminance", model, instance);
        let updated = next(&restored, scope("account", "home"), vec![changed_range]);
        assert!(updated.changed);
        assert!(updated.rebuild);
        assert_eq!(
            updated.configuration.devices[0].endpoint,
            restored.devices[0].endpoint
        );
        assert_eq!(
            updated.configuration.devices[0].functions,
            restored.devices[0].functions
        );
    }
}
