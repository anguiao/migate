use super::status;
use crate::{
    app::App,
    device::{
        DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDeviceDescription,
        TemperatureGrid,
    },
    storage::{Store, tests::fixtures},
    terminal::bridge::Context,
    xiaomi::{adapter::tests::app_source::AppSource, capability::tests::model_fixture::native_for},
};
use futures_lite::future::block_on;
use std::rc::Rc;

fn description(did: &str, kind: FunctionKind, id: &str) -> PhysicalDeviceDescription {
    PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", did),
        did,
        vec![FunctionDescription {
            id: id.into(),
            label: id.into(),
            kind,
        }],
    )
    .unwrap()
}

fn setup(store: &Store) -> AppSource {
    setup_with_sensor_range(store, MeasurementRange::new(-30.0, 100.0).unwrap())
}

fn setup_with_sensor_range(store: &Store, sensor_range: MeasurementRange) -> AppSource {
    let devices = [
        description(
            "ac",
            FunctionKind::AirConditioner {
                temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                vertical_swing: true,
            },
            "air-conditioner:3",
        ),
        description(
            "sensor",
            FunctionKind::Temperature(sensor_range),
            "temperature:3:1001",
        ),
        description(
            "ac2",
            FunctionKind::AirConditioner {
                temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                vertical_swing: true,
            },
            "air-conditioner:3",
        ),
        description(
            "sensor2",
            FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            "temperature:3:1001",
        ),
        description("outlet", FunctionKind::Outlet, "outlet:2"),
    ];
    let mut configuration = store.devices().load_configuration().unwrap();
    configuration.scope = Some(fixtures::scope());
    for (index, description) in devices.iter().enumerate() {
        let native = native_for(description);
        let mut identity = fixtures::identity(
            &native.physical_description(description.label()),
            2 + index as u16 * 2,
        );
        configuration.descriptions.push(native.raw.clone());
        identity.native = Some(native.native);
        configuration.devices.push(identity);
    }
    fixtures::save(store, &configuration);
    let source = AppSource::new(store, None);
    source.confirm_all();
    source
}

#[test]
fn terminal_binding_projects_the_current_sensor_and_persists_without_a_publication_change() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let source = setup_with_sensor_range(&store, MeasurementRange::new(-30.0, 400.0).unwrap());
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = status(&store);
        let terminal = Context::committed(app.clone(), &status);
        let before = app.current().configuration.clone();
        let source_event = app.listen();
        let published = app
            .frozen()
            .into_iter()
            .find(|entry| entry.device.key().id == "ac")
            .unwrap();
        let sensor = app
            .current()
            .devices()
            .into_iter()
            .find(|device| device.key().id == "sensor")
            .unwrap()
            .function("temperature:3:1001")
            .unwrap();
        sensor.set_temperature_source_available(true);
        sensor.observe_measurement(21.5, Some(100), 200).unwrap();
        status.update_adapter(crate::xiaomi::adapter::AdapterStatus {
            authentication: crate::xiaomi::auth::AuthReport::for_test(
                crate::xiaomi::auth::AuthenticationState::Authenticated,
                None,
                crate::xiaomi::auth::CertificateUpdate::NotNeeded,
                0,
            ),
            gateway: crate::xiaomi::adapter::GatewayStatus::Discovering,
            catalog_problem: None,
            pending: Vec::new(),
            routing: Default::default(),
            devices: vec![crate::xiaomi::adapter::DeviceStatus {
                lan: None,
                direct_unavailable_reason: Some("device model not supported".into()),
                did: "sensor".into(),
                read_only: true,
                usable_control_paths: Vec::new(),
                selected_control: None,
                usable_read_paths: vec![crate::xiaomi::adapter::PathKind::Local],
                selected_read: Some(crate::xiaomi::adapter::PathKind::Local),
                physical_online: None,
                notification_source: None,
                unavailable_reason: None,
                sync_limited_reason: None,
                battery_problem: None,
                last_switch_reason: None,
                commands: Vec::new(),
                functions: vec![crate::xiaomi::adapter::FunctionStatus {
                    function: "temperature:3:1001".into(),
                    selected_read: Some(crate::xiaomi::adapter::PathKind::Local),
                    notification_source: None,
                    sync_limited_reason: Some("subscription rejected".into()),
                    read_problem: Some("temporary read failure".into()),
                    observation_source: Some(crate::xiaomi::adapter::PathKind::Local),
                    subscriptions: Vec::new(),
                    properties: Vec::new(),
                }],
            }],
        });
        let help = run(&terminal, "help").await;
        assert!(help.contains("bind-sensor TARGET_DID TARGET_FUNCTION SENSOR_DID SENSOR_FUNCTION"));
        assert!(help.contains("unbind-sensor TARGET_DID TARGET_FUNCTION"));
        let output = terminal
            .handle_line(
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001",
                0,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("bound"), "{output}");
        let output = terminal.handle_line("status ac", 0).await.unwrap().unwrap();
        assert!(output.contains("Target temperature"), "{output}");
        assert!(output.contains("Ambient temperature: 21.50 °C"), "{output}");
        assert!(output.contains("sensor/temperature:3:1001"), "{output}");
        assert!(output.contains("upstream time 100"), "{output}");
        assert!(
            output.contains("selected read hub; notification source none"),
            "{output}"
        );
        assert!(output.contains("read: temporary read failure"), "{output}");
        assert!(
            output.contains("synchronization limited: subscription rejected"),
            "{output}"
        );
        assert_eq!(
            app.current().configuration.root_version,
            before.root_version
        );
        assert_eq!(app.current().configuration.devices, before.devices);
        assert_eq!(
            app.frozen()
                .into_iter()
                .find(|entry| entry.device.key().id == "ac")
                .unwrap()
                .identity,
            published.identity
        );
        assert!(matches!(
            published
                .device
                .function("air-conditioner:3")
                .unwrap()
                .snapshot()
                .ambient_temperature(),
            Some(crate::device::AmbientTemperature::Available(reading)) if reading.value == Some(21.5)
        ));
        assert!(
            futures_lite::future::poll_once(source_event)
                .await
                .is_none()
        );
        assert_eq!(
            store.devices().load_configuration().unwrap(),
            app.current().configuration
        );
        let repeated = terminal
            .handle_line(
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001",
                0,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(repeated.contains("already bound"), "{repeated}");
        let native = before
            .device(&DeviceKey::new("xiaomi", "sensor"))
            .unwrap()
            .native
            .as_ref()
            .unwrap();
        let measurement = native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == "temperature:3:1001")
            .unwrap()
            .measurement()
            .unwrap();
        assert_eq!(measurement.read_value(&serde_json::json!(400.0)), Ok(400.0));
        sensor.observe_measurement(400.0, Some(101), 201).unwrap();
        let output = terminal.handle_line("status ac", 0).await.unwrap().unwrap();
        assert!(
            output.contains("Ambient temperature: 400.00 °C"),
            "{output}"
        );
        assert!(
            output.contains(
                "Matter mapping unsupported: native ambient temperature cannot be represented"
            ),
            "{output}"
        );
    });
}

fn function(app: &App, did: &str, id: &str) -> crate::device::Function {
    app.current()
        .devices()
        .into_iter()
        .find(|device| device.key().id == did)
        .unwrap()
        .function(id)
        .unwrap()
}

async fn run(terminal: &Context<'_>, line: &str) -> String {
    terminal.handle_line(line, 0).await.unwrap().unwrap()
}

#[test]
fn validation_multiple_targets_and_failed_saves_preserve_existing_projections() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let source = setup(&store);
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = status(&store);
        let terminal = Context::committed(app.clone(), &status);
        let sensor = function(&app, "sensor", "temperature:3:1001");
        let second = function(&app, "sensor2", "temperature:3:1001");
        sensor.set_temperature_source_available(true);
        second.set_temperature_source_available(true);
        sensor.observe_measurement(21.0, Some(1), 2).unwrap();
        second.observe_measurement(25.0, Some(3), 4).unwrap();
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        assert!(
            run(
                &terminal,
                "bind-sensor ac2 air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        let saved = store.devices().load_configuration().unwrap();
        for line in [
            "bind-sensor missing air-conditioner:3 sensor temperature:3:1001",
            "bind-sensor ac missing sensor temperature:3:1001",
            "bind-sensor outlet outlet:2 sensor temperature:3:1001",
            "bind-sensor ac air-conditioner:3 missing temperature:3:1001",
            "bind-sensor ac air-conditioner:3 sensor missing",
            "bind-sensor ac air-conditioner:3 outlet outlet:2",
        ] {
            let response = run(&terminal, line).await;
            assert!(response.contains("failed"), "{line}: {response}");
            assert_eq!(store.devices().load_configuration().unwrap(), saved);
        }
        source.forbid("sensor2");
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor2 temperature:3:1001"
            )
            .await
            .contains("no current access")
        );
        assert_eq!(store.devices().load_configuration().unwrap(), saved);
        source.allow("sensor2");
        // A failed write leaves the old relation live; repeated same-source binding is a no-op.
        rusqlite::Connection::open(store.path()).unwrap().execute_batch(
            "CREATE TRIGGER reject_binding AFTER UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'injected binding failure'); END;"
        ).unwrap();
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("already bound")
        );
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor2 temperature:3:1001"
            )
            .await
            .contains("failed")
        );
        assert!(
            run(&terminal, "unbind-sensor ac air-conditioner:3")
                .await
                .contains("failed")
        );
        assert_eq!(store.devices().load_configuration().unwrap(), saved);
        sensor.observe_measurement(22.0, Some(5), 6).unwrap();
        for did in ["ac", "ac2"] {
            let output = run(&terminal, &format!("status {did}")).await;
            assert!(output.contains("Ambient temperature: 22.00 °C"), "{output}");
            assert!(
                function(&app, did, "air-conditioner:3")
                    .pending_air_conditioner_commands()
                    .is_empty()
            );
        }
    });
}

#[test]
fn replacement_unbinding_and_missing_source_restore_follow_saved_relationships() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let source = setup(&store);
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = status(&store);
        let terminal = Context::committed(app.clone(), &status);
        let first = function(&app, "sensor", "temperature:3:1001");
        let second = function(&app, "sensor2", "temperature:3:1001");
        first.set_temperature_source_available(true);
        first.observe_measurement(20.0, Some(10), 11).unwrap();
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor2 temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        let output = run(&terminal, "status ac").await;
        assert!(
            output.contains("unknown (temperature source unavailable)"),
            "{output}"
        );
        second.set_temperature_source_available(true);
        second.observe_measurement(24.0, Some(12), 13).unwrap();
        assert!(
            run(&terminal, "status ac")
                .await
                .contains("Ambient temperature: 24.00 °C")
        );
        second.set_temperature_source_available(false);
        let output = run(&terminal, "status ac").await;
        assert!(
            output.contains("unknown (temperature source unavailable)"),
            "{output}"
        );
        assert!(
            output.contains("Last reading (diagnostic only): 24.00 °C"),
            "{output}"
        );
        assert!(
            output.contains("Ambient source synchronization:"),
            "{output}"
        );
        source.remove("sensor2");
        let plan = app.plan().unwrap().unwrap();
        assert_eq!(plan.configuration.temperature_bindings.len(), 1);
        app.apply(app.prepare(plan)).unwrap();
        let output = run(&terminal, "status ac").await;
        assert!(
            output.contains("source not in current directory or access revoked"),
            "{output}"
        );
        assert!(output.contains("sensor2/temperature:3:1001"), "{output}");
        source.describe(description(
            "sensor2",
            FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            "temperature:3:1001",
        ));
        let plan = app.plan().unwrap().unwrap();
        app.apply(app.prepare(plan)).unwrap();
        let restored = function(&app, "sensor2", "temperature:3:1001");
        restored.set_temperature_source_available(true);
        let output = run(&terminal, "status ac").await;
        assert!(
            output.contains("awaiting a valid temperature reading"),
            "{output}"
        );
        restored.observe_measurement(26.0, Some(15), 16).unwrap();
        assert!(
            run(&terminal, "status ac")
                .await
                .contains("Ambient temperature: 26.00 °C")
        );
        assert!(
            run(&terminal, "unbind-sensor ac air-conditioner:3")
                .await
                .contains("unbound")
        );
        rusqlite::Connection::open(store.path()).unwrap().execute_batch(
            "CREATE TRIGGER reject_repeat AFTER UPDATE ON bridge_configuration BEGIN SELECT RAISE(ABORT, 'unexpected write'); END;"
        ).unwrap();
        assert!(
            run(&terminal, "unbind-sensor ac air-conditioner:3")
                .await
                .contains("already unbound")
        );
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute_batch("DROP TRIGGER reject_repeat;")
            .unwrap();
        assert!(
            run(&terminal, "status ac")
                .await
                .contains("unknown (not bound)")
        );

        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        assert!(
            run(
                &terminal,
                "bind-sensor ac2 air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("bound")
        );
        source.remove("sensor");
        let plan = app.plan().unwrap().unwrap();
        app.apply(app.prepare(plan)).unwrap();
        assert!(
            run(
                &terminal,
                "bind-sensor ac air-conditioner:3 sensor temperature:3:1001"
            )
            .await
            .contains("already bound")
        );
        assert!(
            run(&terminal, "unbind-sensor ac2 air-conditioner:3")
                .await
                .contains("unbound")
        );
        source.remove("ac");
        let plan = app.plan().unwrap().unwrap();
        app.apply(app.prepare(plan)).unwrap();
        assert!(
            run(&terminal, "unbind-sensor ac air-conditioner:3")
                .await
                .contains("unbound")
        );
        assert!(
            run(&terminal, "unbind-sensor ac air-conditioner:3")
                .await
                .contains("already unbound")
        );
    });
}

#[test]
fn bindings_survive_restart_and_directory_changes_but_not_a_scope_change() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let source = setup(&store);
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = status(&store);
        let terminal = Context::committed(app.clone(), &status);
        let sensor = function(&app, "sensor", "temperature:3:1001");
        sensor.set_temperature_source_available(true);
        sensor.observe_measurement(18.0, Some(20), 21).unwrap();
        run(
            &terminal,
            "bind-sensor ac air-conditioner:3 sensor temperature:3:1001",
        )
        .await;
        let saved = store.devices().load_configuration().unwrap();
        let target = saved.temperature_bindings.keys().next().unwrap().clone();
        let identity = saved.device(&target.device).unwrap().clone();
        source.directory_name(&DeviceKey::new("xiaomi", "sensor"), "New name");
        let plan = app.plan().unwrap().unwrap();
        assert!(!plan.rebuild);
        app.apply(app.prepare(plan)).unwrap();
        assert_eq!(
            app.current().configuration.temperature_bindings,
            saved.temperature_bindings
        );
        assert_eq!(
            app.current().configuration.device(&target.device).unwrap(),
            &identity
        );
        drop(terminal);
        drop(app);
        drop(source);
        drop(store);

        let store = Store::open(directory.path()).unwrap();
        let source = AppSource::new(&store, None);
        source.confirm_all();
        let app = Rc::new(App::open(source.handle.clone(), store.clone()).unwrap());
        let status = super::status(&store);
        let terminal = Context::committed(app.clone(), &status);
        let output = run(&terminal, "status ac").await;
        assert!(output.contains("sensor/temperature:3:1001"), "{output}");
        assert!(
            output.contains("unknown (temperature source unavailable)"),
            "{output}"
        );
        assert!(!output.contains("18.00 °C"), "{output}");
        for scope in [
            crate::configuration::Scope {
                uid: "account".into(),
                home_id: "different".into(),
            },
            crate::configuration::Scope {
                uid: "different".into(),
                home_id: "home".into(),
            },
        ] {
            source.scope(scope);
            let plan = app.plan().unwrap().unwrap();
            assert!(plan.configuration.temperature_bindings.is_empty());
        }
    });
}
