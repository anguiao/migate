//! Sensor capability persistence in the current schema-8 private codec.

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
        [("temperature", 3), ("humidity", 4)]
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
    assert_eq!(functions[0]["measurement"]["step"], json!(0.1));
    assert_eq!(functions[1]["kind"], json!("humidity"));
    assert_eq!(functions[1]["measurement"]["unit"], json!("percentage"));
    // This slice does not model battery or persist its observations.
    assert!(encoded["devices"][0].get("battery").is_none());
    assert!(encoded["devices"][0]["native"].get("battery").is_none());
    for forbidden in ["value", "source_time", "received_at", "reading"] {
        assert!(functions[0].get(forbidden).is_none(), "{forbidden}");
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
    assert_eq!(sensor.endpoint, 3);
    assert_eq!(
        sensor
            .functions
            .iter()
            .map(|f| f.endpoint)
            .collect::<Vec<_>>(),
        [4, 5]
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
fn a_schema8_sensor_record_with_an_unknown_battery_field_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::storage::Store::open(dir.path()).unwrap();
    seed(&store, vec![sensor("sensor")]);
    let valid: Value = serde_json::from_str(&raw(&store)).unwrap();
    let mut changed = valid.clone();
    changed["devices"][0]["battery"] = json!({"level": 50});
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
        ("/devices/0/native/functions/0/measurement/step", json!(0.0)),
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
    assert_eq!(measurement.step(), 0.1);
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
        measurement.step(),
    )
    .unwrap();
    let changed = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        ParsedDevice::new(
            original.device().key().clone(),
            vec![
                ParsedFunction::temperature("temperature", "Temperature", changed_binding),
                functions[1].clone(),
            ],
            None,
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
fn a_measurement_step_only_change_does_not_rebuild_the_published_instance() {
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
    let root = first.root_version;

    let functions = original.device().functions().to_vec();
    let measurement = functions[0].measurement().unwrap();
    let changed_binding = MeasurementBinding::new(
        measurement.property(),
        measurement.format(),
        measurement.unit(),
        MeasurementRange::new(-30.0, 100.0).unwrap(),
        0.01,
    )
    .unwrap();
    let changed = crate::xiaomi::capability::NativeDescription::new(
        original.model().into(),
        original.instance_type().into(),
        ParsedDevice::new(
            original.device().key().clone(),
            vec![
                ParsedFunction::temperature("temperature", "Temperature", changed_binding),
                functions[1].clone(),
            ],
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let plan = next(&first, scope("account", "home"), vec![changed]);
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
