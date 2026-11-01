use super::*;
use crate::device::{
    AirConditionerCommand as Command, AirConditionerFanSpeed as Speed, AirConditionerMode as Mode,
    AirConditionerStep as Step, AirConditionerValue as Value, CommandReceiver, DeviceKey,
    DeviceRequest, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDevice,
    PhysicalDeviceDescription, TemperatureGrid,
};

fn device() -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = PhysicalDevice::new(
        PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", "subscription-ac"),
            "AC",
            vec![FunctionDescription {
                id: "air-conditioner:3".into(),
                label: "AC".into(),
                kind: FunctionKind::AirConditioner {
                    temperature: TemperatureGrid::new(16, 30, 1).unwrap(),
                    vertical_swing: true,
                },
            }],
        )
        .unwrap(),
    );
    device.set_availability(Availability::Available);
    (device, receivers.remove(0))
}

#[test]
fn locked_stack_ambient_subscription_tracks_values_null_and_ignores_time_only_changes() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 96);
            connect(&client, CLIENT_ID, SERVER_ID, 96);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, mut receiver) = device();
            let function = &device.functions()[0];
            let sensor_device = PhysicalDevice::new(
                PhysicalDeviceDescription::new(
                    DeviceKey::new("xiaomi", "sensor"),
                    "Sensor",
                    vec![FunctionDescription {
                        id: "temperature:3:7".into(),
                        label: "Temperature".into(),
                        kind: FunctionKind::Temperature(
                            MeasurementRange::new(-30.0, 100.0).unwrap(),
                        ),
                    }],
                )
                .unwrap(),
            )
            .0;
            let sensor = &sensor_device.functions()[0];
            let handler = model_handler(vec![single_entry(&device)]);
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let incoming = Pipe::default();
            let outgoing = Pipe::default();
            let responder = DefaultResponder::new(&im);
            block_on(async {
                im.startup().await.unwrap();
                let services = async {
                    or(
                        matter.run(
                            &crypto,
                            SendPipe(&outgoing),
                            ReceivePipe(&incoming),
                            NoNetwork,
                        ),
                        or(
                            client.run(
                                &crypto,
                                SendPipe(&incoming),
                                ReceivePipe(&outgoing),
                                NoNetwork,
                            ),
                            or(responder.run::<4, 4>(), im.run()),
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("ambient subscription services exited");
                };
                let controller = async {
                    let paths = [AttrPath::from_gp(&GenericPath::new(
                        Some(2),
                        Some(0x201),
                        Some(0),
                    ))];
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    let (mut version, value) = initial.ambient_temperature.unwrap();
                    assert_eq!(value, None);
                    wait_committed(&state).await;
                    function.set_ambient_temperature_source(Some((
                        sensor.function_key().clone(),
                        Some(sensor.clone()),
                    )));
                    sensor.set_temperature_source_available(true);
                    sensor.observe_measurement(0.0, Some(1), 1).unwrap();
                    let zero = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(zero.1, Some(0));
                    assert_ne!(zero.0, version);
                    version = zero.0;
                    wait_committed(&state).await;
                    sensor.observe_measurement(21.5, Some(2), 2).unwrap();
                    let next = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(next.1, Some(2150));
                    assert_ne!(next.0, version);
                    version = next.0;
                    wait_committed(&state).await;
                    sensor.observe_measurement(21.5, Some(3), 3).unwrap();
                    or(
                        async {
                            let report = receive_observed(&client, id).await.unwrap();
                            panic!("unexpected time-only report: {report:?}");
                        },
                        async { async_io::Timer::after(Duration::from_millis(200)).await },
                    )
                    .await;
                    sensor.set_temperature_source_available(false);
                    let unknown = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(unknown.1, None);
                    assert_ne!(unknown.0, version);
                    version = unknown.0;
                    wait_committed(&state).await;
                    sensor.set_temperature_source_available(true);
                    sensor.observe_measurement(23.0, Some(4), 4).unwrap();
                    let restored = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(restored.1, Some(2300));
                    assert_ne!(restored.0, version);
                    version = restored.0;
                    wait_committed(&state).await;
                    function.set_ambient_temperature_source(None);
                    let unbound = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(unbound.1, None);
                    assert_ne!(unbound.0, version);
                    wait_committed(&state).await;
                    function.set_ambient_temperature_source(Some((
                        sensor.function_key().clone(),
                        Some(sensor.clone()),
                    )));
                    let rebound = receive_observed(&client, id)
                        .await
                        .unwrap()
                        .ambient_temperature
                        .unwrap();
                    assert_eq!(rebound.1, Some(2300));
                    assert_ne!(rebound.0, unbound.0);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(8)).await;
                        panic!("ambient subscription timed out");
                    }),
                )
                .await;
            });
            protocol.check_failure().unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn locked_stack_subscription_tracks_observations_across_both_clusters_without_receipt_updates() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 93);
            connect(&client, CLIENT_ID, SERVER_ID, 93);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, mut receiver) = device();
            let function = &device.functions()[0];
            for value in [
                Value::Power(true),
                Value::Mode(Mode::Cool),
                Value::Temperature(24),
                Value::Speed(Speed::Low),
                Value::Swing(false),
            ] {
                function.observe_air_conditioner(value, Some(1), 1);
            }
            let handler = model_handler(vec![single_entry(&device)]);
            let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
            let incoming = Pipe::default();
            let outgoing = Pipe::default();
            let responder = DefaultResponder::new(&im);
            block_on(async {
                im.startup().await.unwrap();
                let services = async {
                    or(
                        matter.run(
                            &crypto,
                            SendPipe(&outgoing),
                            ReceivePipe(&incoming),
                            NoNetwork,
                        ),
                        or(
                            client.run(
                                &crypto,
                                SendPipe(&incoming),
                                ReceivePipe(&outgoing),
                                NoNetwork,
                            ),
                            or(responder.run::<4, 4>(), im.run()),
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("air-conditioner subscription services exited");
                };
                let controller = async {
                    let paths = [
                        (6, 0),
                        (0x201, 17),
                        (0x201, 18),
                        (0x201, 28),
                        (0x202, 0),
                        (0x202, 2),
                        (0x202, 3),
                        (0x202, 5),
                        (0x202, 6),
                        (0x202, 8),
                    ]
                    .map(|(cluster, attribute)| {
                        AttrPath::from_gp(&GenericPath::new(
                            Some(2),
                            Some(cluster),
                            Some(attribute),
                        ))
                    });
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert!(initial.powers[&2].1);
                    assert_eq!(initial.thermostat[&17].1, 2400);
                    assert_eq!(initial.thermostat[&18].1, 2400);
                    assert_eq!(initial.thermostat[&28].1, 3);
                    assert_eq!(initial.fan[&0].1, 1);
                    let action = function
                        .begin_air_conditioner(Command::Temperature(25))
                        .unwrap();
                    let DeviceRequest::AirConditioner(request) =
                        receiver.next_operation().await.unwrap()
                    else {
                        panic!("expected air-conditioner request")
                    };
                    request.mark_sent(Step::Temperature(25)).unwrap();
                    request.step_accepted(Step::Temperature(25)).unwrap();
                    request.accept().unwrap();
                    action.accepted().await.unwrap();
                    or(
                        async {
                            let report = receive_observed(&client, id).await.unwrap();
                            panic!("unexpected receipt report: {report:?}");
                        },
                        async { async_io::Timer::after(Duration::from_millis(200)).await },
                    )
                    .await;
                    function.observe_air_conditioner(Value::Temperature(25), Some(2), 2);
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_eq!(temperature.thermostat[&17].1, 2500);
                    assert_eq!(temperature.thermostat[&18].1, 2500);
                    assert_ne!(temperature.thermostat[&17].0, initial.thermostat[&17].0);
                    wait_committed(&state).await;
                    function.observe_air_conditioner(Value::Mode(Mode::Heat), Some(3), 3);
                    function.observe_air_conditioner(Value::Speed(Speed::High), Some(3), 3);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(changed.thermostat[&28].1, 4);
                    assert_eq!(changed.fan[&0].1, 3);
                    let mut sequence = 3;
                    let mut fan_version = changed.fan[&0].0;
                    for _ in 0..3 {
                        for (speed, level, percent) in [
                            (Speed::Low, 1, 33),
                            (Speed::Medium, 2, 66),
                            (Speed::High, 3, 100),
                        ] {
                            wait_committed(&state).await;
                            sequence += 1;
                            function.observe_air_conditioner(
                                Value::Speed(speed),
                                Some(sequence),
                                sequence,
                            );
                            let report = receive_observed(&client, id).await.unwrap();
                            for (attribute, expected) in [
                                (0, level),
                                (2, percent),
                                (3, percent),
                                (5, level),
                                (6, level),
                            ] {
                                assert_eq!(report.fan[&attribute].1, expected);
                                assert_ne!(report.fan[&attribute].0, fan_version);
                                assert_eq!(report.fan[&attribute].0, report.fan[&0].0);
                            }
                            fan_version = report.fan[&0].0;
                        }
                        for swing in [true, false] {
                            wait_committed(&state).await;
                            sequence += 1;
                            function.observe_air_conditioner(
                                Value::Swing(swing),
                                Some(sequence),
                                sequence,
                            );
                            let report = receive_observed(&client, id).await.unwrap();
                            assert_eq!(report.fan[&8].1, if swing { 2 } else { 0 });
                            assert_ne!(report.fan[&8].0, fan_version);
                            fan_version = report.fan[&8].0;
                        }
                    }
                    // Auto reports a known mode while its current speed remains unknown.
                    let auto_paths = paths
                        .iter()
                        .filter(|path| {
                            path.cluster != Some(0x202) || matches!(path.attr, Some(0 | 8))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    let (auto_id, _) = subscribe_paths(&client, &auto_paths).await.unwrap();
                    wait_committed(&state).await;
                    sequence += 1;
                    function.observe_air_conditioner(
                        Value::Mode(Mode::Cool),
                        Some(sequence),
                        sequence,
                    );
                    function.observe_air_conditioner(
                        Value::Speed(Speed::Auto),
                        Some(sequence),
                        sequence,
                    );
                    let automatic = receive_observed(&client, auto_id).await.unwrap();
                    assert_eq!(automatic.thermostat[&28].1, 3);
                    assert_eq!(automatic.fan[&0].1, 5);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(8)).await;
                        panic!("air-conditioner subscription timed out");
                    }),
                )
                .await;
            });
            protocol.check_failure().unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}

fn persistent_boot(directory: &Path, boot: u16, previous: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    if previous.is_some() {
        assert!(matter.has_fabrics());
    }
    connect(&matter, SERVER_ID, CLIENT_ID, boot);
    connect(&client, CLIENT_ID, SERVER_ID, boot);
    if previous.is_none() {
        let mut fabrics = rs_matter::fabric::FabricPersist::new(&kv);
        matter.with_state(|state| {
            fabrics
                .store(state.fabrics.fabric(NonZeroU8::new(1).unwrap()).unwrap())
                .unwrap()
        });
        fabrics.run().unwrap();
    }
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let (device, mut receiver) = device();
    let function = &device.functions()[0];
    let (mode, temperature, speed) = if previous.is_some() {
        (Mode::Heat, 26, Speed::High)
    } else {
        (Mode::Cool, 24, Speed::Low)
    };
    for value in [
        Value::Power(true),
        Value::Mode(mode),
        Value::Temperature(temperature),
        Value::Speed(speed),
        Value::Swing(false),
    ] {
        function.observe_air_conditioner(value, Some(1), 1);
    }
    let handler = model_handler(vec![single_entry(&device)]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let id = block_on(async {
        im.startup().await.unwrap();
        if previous.is_some() {
            assert!(
                state
                    .subscriptions()
                    .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
            );
        }
        let services = async {
            or(
                matter.run(
                    &crypto,
                    SendPipe(&outgoing),
                    ReceivePipe(&incoming),
                    NoNetwork,
                ),
                or(
                    client.run(
                        &crypto,
                        SendPipe(&incoming),
                        ReceivePipe(&outgoing),
                        NoNetwork,
                    ),
                    or(responder.run::<4, 4>(), im.run()),
                ),
            )
            .await
            .unwrap();
            panic!("air-conditioner recovery services exited");
        };
        let controller = async {
            assert!(
                futures_lite::future::poll_once(receiver.next_operation())
                    .await
                    .is_none()
            );
            if let Some(id) = previous {
                let restored = receive_observed(&client, id).await.unwrap();
                assert!(restored.powers[&2].1);
                assert_eq!(restored.thermostat[&17].1, 2600);
                assert_eq!(restored.thermostat[&18].1, 2600);
                assert_eq!(restored.thermostat[&28].1, 4);
                assert_eq!(restored.fan[&0].1, 3);
                assert!(
                    futures_lite::future::poll_once(receiver.next_operation())
                        .await
                        .is_none()
                );
                wait_committed(&state).await;
                function.observe_air_conditioner(Value::Temperature(27), Some(2), 2);
                let changed = receive_observed(&client, id).await.unwrap();
                assert_eq!(changed.thermostat[&17].1, 2700);
                assert_eq!(changed.thermostat[&18].1, 2700);
                assert!(
                    futures_lite::future::poll_once(receiver.next_operation())
                        .await
                        .is_none()
                );
                id
            } else {
                let paths = [(6, 0), (0x201, 17), (0x201, 18), (0x201, 28), (0x202, 0)].map(
                    |(cluster, attribute)| {
                        AttrPath::from_gp(&GenericPath::new(
                            Some(2),
                            Some(cluster),
                            Some(attribute),
                        ))
                    },
                );
                let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                assert_eq!(initial.thermostat[&17].1, 2400);
                assert_eq!(initial.fan[&0].1, 1);
                wait_committed(&state).await;
                let action = function
                    .begin_air_conditioner(Command::Temperature(25))
                    .unwrap();
                let DeviceRequest::AirConditioner(request) =
                    receiver.next_operation().await.unwrap()
                else {
                    panic!("expected air-conditioner request")
                };
                request.mark_sent(Step::Temperature(25)).unwrap();
                request.step_accepted(Step::Temperature(25)).unwrap();
                request.accept().unwrap();
                action.accepted().await.unwrap();
                id
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(8)).await;
                panic!("air-conditioner recovery timed out");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    id
}

#[test]
fn persistent_subscription_restores_new_observations_without_replaying_commands() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let identity = Store::open(directory.path())
                .unwrap()
                .load_identity()
                .unwrap();
            let id = persistent_boot(directory.path(), 94, None);
            persistent_boot(directory.path(), 95, Some(id));
            assert_eq!(
                Store::open(directory.path())
                    .unwrap()
                    .load_identity()
                    .unwrap(),
                identity
            );
        })
        .unwrap()
        .join()
        .unwrap();
}
