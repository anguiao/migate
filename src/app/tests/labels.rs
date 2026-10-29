use super::{outlet, status};
use crate::{
    app::App, storage::Store, terminal::bridge::Context,
    xiaomi::adapter::tests::app_source::AppSource,
};
use futures_lite::future::block_on;
use std::rc::Rc;

#[test]
fn empty_saved_name_survives_restart_and_later_directory_names() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let key = outlet().key().clone();
    let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
    let before = app.current().configuration.device(&key).unwrap().clone();
    let physical = source.physical.as_ref().unwrap();
    let status = status(&store);
    let terminal = Context::committed(app.clone(), &status);
    assert!(
        block_on(terminal.handle_line("list", 0))
            .unwrap()
            .unwrap()
            .contains("Test Outlet")
    );

    app.rename(&key, "").unwrap();
    assert_eq!(physical.description().label(), "");
    assert_eq!(app.current().derived.served[&key].description.label(), "");
    assert_eq!(app.frozen()[0].device.description().label(), "");
    assert!(
        !block_on(terminal.handle_line("list", 0))
            .unwrap()
            .unwrap()
            .contains("Test Outlet")
    );
    source.directory_name(&key, "Directory rename");
    assert!(app.plan().unwrap().is_none());
    let saved = store.devices().load_configuration().unwrap();
    let renamed = saved.device(&key).unwrap();
    assert_eq!(renamed.label, "");
    assert_eq!(renamed.unique_id, before.unique_id);
    assert_eq!(renamed.endpoint, before.endpoint);
    assert_eq!(renamed.configuration_version, before.configuration_version);
    drop(terminal);
    drop(app);
    drop(source);
    drop(store);

    let store = Store::open(directory.path()).unwrap();
    let restored = AppSource::new(&store, None);
    let app = App::open(restored.handle.clone(), store.clone()).unwrap();
    assert_eq!(
        restored.physical.as_ref().unwrap().description().label(),
        ""
    );
    assert_eq!(app.current().configuration.device(&key).unwrap().label, "");
    assert!(app.plan().unwrap().is_none());
}

#[test]
fn rename_rejects_overlong_utf8_without_changing_saved_or_served_views() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let key = outlet().key().clone();
    let app = App::open(source.handle.clone(), store.clone()).unwrap();
    let valid = "灯".repeat(10);
    app.rename(&key, &valid).unwrap();
    let saved = store.devices().load_configuration().unwrap();
    assert!(app.rename(&key, &"灯".repeat(11)).is_err());
    assert_eq!(store.devices().load_configuration().unwrap(), saved);
    assert_eq!(app.current().configuration, saved);
    assert_eq!(
        source.physical.as_ref().unwrap().description().label(),
        valid
    );
}

#[test]
fn failed_name_commit_keeps_the_saved_and_served_name() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let source = AppSource::new(&store, Some(outlet()));
    let key = outlet().key().clone();
    let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
    let saved = store.devices().load_configuration().unwrap();
    rusqlite::Connection::open(store.path()).unwrap().execute_batch(
        "CREATE TRIGGER reject_name AFTER UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected rename failure'); END;"
    ).unwrap();
    assert!(app.rename(&key, "Changed").is_err());
    assert_eq!(store.devices().load_configuration().unwrap(), saved);
    assert_eq!(app.current().configuration, saved);
    assert_eq!(
        source.physical.as_ref().unwrap().description().label(),
        "Test Outlet"
    );
    let status = status(&store);
    let terminal = Context::committed(app, &status);
    assert!(
        block_on(terminal.handle_line("list", 0))
            .unwrap()
            .unwrap()
            .contains("Test Outlet")
    );
}

#[test]
fn renaming_a_sensor_keeps_its_battery_capability() {
    use crate::device::{FunctionKind, MeasurementRange};

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let sensor = crate::device::PhysicalDeviceDescription::new(
        crate::device::DeviceKey::new("xiaomi", "sensor"),
        "Sensor",
        vec![
            crate::device::FunctionDescription {
                id: "temperature:3:1001".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            },
            crate::device::FunctionDescription {
                id: "humidity".into(),
                label: "Humidity".into(),
                kind: FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
            },
        ],
    )
    .unwrap()
    .with_battery();
    let source = AppSource::new(&store, Some(sensor));
    let key = source.physical.as_ref().unwrap().key().clone();
    let app = App::open(source.handle.clone(), store.clone()).unwrap();
    app.rename(&key, "Renamed sensor").unwrap();
    assert!(app.current().derived.served[&key].description.has_battery());
    assert!(
        source
            .physical
            .as_ref()
            .unwrap()
            .description()
            .has_battery()
    );
    assert_eq!(
        crate::matter::fixed_shape_with_battery(
            app.current().derived.served[&key]
                .description
                .functions()
                .iter(),
            true
        ),
        crate::matter::fixed_shape_with_battery(
            source
                .physical
                .as_ref()
                .unwrap()
                .description()
                .functions()
                .iter(),
            true
        )
    );
}
