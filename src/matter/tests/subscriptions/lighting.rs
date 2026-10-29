use super::*;

#[test]
fn lighting_subscription_reports_active_color_groups_modes_and_shared_versions() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            use crate::device::{LightMode, LightPatch, Rgb};
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let info = basic_info(&identity);
            let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let protocol = StoreAdapter::new(store.matter());
            let kv = matter.kv(protocol.clone());
            matter.startup(&kv).unwrap();
            let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            connect(&matter, SERVER_ID, CLIENT_ID, 31);
            connect(&client, CLIENT_ID, SERVER_ID, 31);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, _receiver) = super::super::support::light(true);
            device.set_availability(Availability::Available);
            let function = device.functions()[0].clone();
            function
                .observe_light(LightPatch {
                    power: Some(true),
                    brightness_percent: Some(50.0),
                    temperature_kelvin: Some(6500),
                    color: Some(Rgb {
                        red: 255,
                        green: 0,
                        blue: 0,
                    }),
                    mode: Some(LightMode::Rgb),
                })
                .unwrap();
            let handler = crate::matter::tests::support::model_handler(vec![
                crate::matter::tests::support::single_entry(&device),
            ]);
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
                    panic!("lighting services exited");
                };
                let controller = async {
                    let assert_colors = |report: &Observed, expected: &[(u32, u16)]| {
                        let colors = report
                            .lighting
                            .iter()
                            .filter(|((endpoint, cluster, _), _)| {
                                *endpoint == 2 && *cluster == 0x300
                            })
                            .map(|((_, _, attribute), (_, value))| (*attribute, *value))
                            .collect::<Vec<_>>();
                        assert_eq!(colors, expected);
                        if let Some((attribute, _)) = expected.first() {
                            let version = report.lighting[&(2, 0x300, *attribute)].0;
                            for (attribute, _) in expected {
                                assert_eq!(report.lighting[&(2, 0x300, *attribute)].0, version);
                            }
                        }
                    };
                    let cool_white = [
                        (0, 218),
                        (1, 7),
                        (3, 20554),
                        (4, 21209),
                        (7, 154),
                        (8, 2),
                        (0x4001, 2),
                    ];
                    let paths = [
                        (8, 0),
                        (0x300, 0),
                        (0x300, 1),
                        (0x300, 3),
                        (0x300, 4),
                        (0x300, 7),
                        (0x300, 8),
                        (0x300, 0x4001),
                    ]
                    .map(|(c, a)| AttrPath::from_gp(&GenericPath::new(Some(2), Some(c), Some(a))));
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        initial.lighting[&(2, 8, 0)].1,
                        crate::matter::units::level_from_brightness(50.0) as u16
                    );
                    assert_eq!(initial.lighting[&(2, 0x300, 0)].1, 0);
                    assert_eq!(initial.lighting[&(2, 0x300, 1)].1, 254);
                    assert_eq!(initial.lighting[&(2, 0x300, 7)].1, 154);
                    assert_eq!(initial.lighting[&(2, 0x300, 8)].1, 0);
                    assert_eq!(initial.lighting[&(2, 0x300, 0x4001)].1, 0);

                    // The active value must accompany a mode-only change even
                    // when its cached value was already included in priming.
                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    assert_ne!(
                        temperature.lighting[&(2, 0x300, 7)].0,
                        initial.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let rgb = receive_observed(&client, id).await.unwrap();
                    let red = [
                        (0, 0),
                        (1, 254),
                        (3, initial.lighting[&(2, 0x300, 3)].1),
                        (4, initial.lighting[&(2, 0x300, 4)].1),
                        (8, 0),
                        (0x4001, 0),
                    ];
                    assert_colors(&rgb, &red);
                    assert_ne!(
                        rgb.lighting[&(2, 0x300, 0)].0,
                        temperature.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(2700),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let warm = receive_observed(&client, id).await.unwrap();
                    assert_colors(
                        &warm,
                        &[
                            (0, 21),
                            (1, 165),
                            (3, 30132),
                            (4, 26930),
                            (7, 370),
                            (8, 2),
                            (0x4001, 2),
                        ],
                    );
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            ..Default::default()
                        })
                        .unwrap();
                    let cool = receive_observed(&client, id).await.unwrap();
                    assert_colors(&cool, &cool_white[..5]);
                    assert_ne!(
                        cool.lighting[&(2, 0x300, 7)].0,
                        warm.lighting[&(2, 0x300, 7)].0
                    );
                    wait_committed(&state).await;

                    // Both Kelvin values encode as 154 mireds. The mode edge
                    // must still report CT, while switching away reports RGB only.
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6494),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let rgb = receive_observed(&client, id).await.unwrap();
                    assert_colors(&rgb, &red);
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    assert_ne!(
                        temperature.lighting[&(2, 0x300, 7)].0,
                        rgb.lighting[&(2, 0x300, 0)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            brightness_percent: Some(23.5),
                            temperature_kelvin: Some(3333),
                            color: Some(Rgb {
                                red: 255,
                                green: 128,
                                blue: 64,
                            }),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    let report = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        report.lighting[&(2, 8, 0)].1,
                        crate::matter::units::level_from_brightness(23.5) as u16
                    );
                    assert_eq!(report.lighting[&(2, 0x300, 3)].1, 33935);
                    assert_eq!(report.lighting[&(2, 0x300, 4)].1, 25220);
                    assert!(!report.lighting.contains_key(&(2, 0x300, 7)));
                    assert_eq!(report.lighting[&(2, 0x300, 0)].1, 14);
                    assert_eq!(report.lighting[&(2, 0x300, 1)].1, 190);
                    assert_eq!(report.lighting[&(2, 0x300, 8)].1, 0);
                    assert_eq!(report.lighting[&(2, 0x300, 0x4001)].1, 0);
                    for attribute in [0, 1, 8, 0x4001] {
                        assert_eq!(
                            report.lighting[&(2, 0x300, attribute)].0,
                            report.lighting[&(2, 0x300, 3)].0,
                        );
                    }
                    assert_ne!(
                        report.lighting[&(2, 8, 0)].0,
                        initial.lighting[&(2, 8, 0)].0
                    );
                    assert_ne!(
                        report.lighting[&(2, 0x300, 3)].0,
                        initial.lighting[&(2, 0x300, 3)].0
                    );
                    wait_committed(&state).await;

                    function
                        .observe_light(LightPatch {
                            temperature_kelvin: Some(6500),
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    let temperature = receive_observed(&client, id).await.unwrap();
                    assert_colors(&temperature, &cool_white);
                    wait_committed(&state).await;

                    // Priming includes every requested attribute. Its HS/XY
                    // must describe the active white light, not the RGB cache.
                    let started = std::time::Instant::now();
                    let (id, priming) = subscribe_paths_with_min_interval(&client, &paths, 2)
                        .await
                        .unwrap();
                    assert_colors(&priming, &cool_white);
                    wait_committed(&state).await;
                    function
                        .observe_light(LightPatch {
                            color: Some(Rgb {
                                red: 255,
                                green: 0,
                                blue: 0,
                            }),
                            mode: Some(LightMode::Rgb),
                            ..Default::default()
                        })
                        .unwrap();
                    // A read verifies the intermediate RGB state without
                    // consuming the subscription's pending attribute changes.
                    let intermediate = read_paths(&client, &paths).await.unwrap();
                    assert_eq!(intermediate.lighting[&(2, 0x300, 0)].1, 0);
                    assert_eq!(intermediate.lighting[&(2, 0x300, 1)].1, 254);
                    assert_eq!(intermediate.lighting[&(2, 0x300, 8)].1, 0);
                    assert!(started.elapsed() < Duration::from_secs(2));
                    function
                        .observe_light(LightPatch {
                            mode: Some(LightMode::Temperature),
                            ..Default::default()
                        })
                        .unwrap();
                    // Both mode changes fall within the same report interval.
                    // Even previously marked RGB attributes must encode the
                    // final CT projection when the actual ReportData is built.
                    let merged = receive_observed(&client, id).await.unwrap();
                    assert_colors(&merged, &cool_white);
                    assert_ne!(
                        merged.lighting[&(2, 0x300, 7)].0,
                        priming.lighting[&(2, 0x300, 7)].0
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(10)).await;
                        panic!("lighting subscription timed out");
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

fn frozen_light_boot(directory: &Path, boot: u16, previous: Option<u32>) -> (u32, Observed) {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let root_version = if boot == 1 { 1 } else { 2 };
    let settings = model::prepare_basic_info(&kv, root_version).unwrap();
    matter.with_state(|state| state.basic_info_settings = settings);
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&matter, SERVER_ID, CLIENT_ID, boot + 30);
    connect(&client, CLIENT_ID, SERVER_ID, boot + 30);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let interval = if boot == 1 {
        (2700, 6500)
    } else {
        (3000, 4000)
    };
    let (device, _receiver) =
        super::super::support::light_with(crate::device::LightProfile::ColorTemperature(
            crate::device::TemperatureInterval::new(interval.0, interval.1).unwrap(),
        ));
    device.set_availability(Availability::Available);
    let mut entry = single_entry(&device);
    entry.identity.configuration_version = root_version;
    let handler = model_handler(vec![entry]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let (id, observed) = block_on(async {
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
            panic!("frozen light services exited");
        };
        let controller = async {
            let paths = [
                (Some(0), 0x28, Some(24)),
                (None, 0x39, Some(24)),
                (None, 0x300, Some(0x400b)),
                (None, 0x300, Some(0x400c)),
            ]
            .map(|(endpoint, cluster, attr)| {
                AttrPath::from_gp(&GenericPath::new(endpoint, Some(cluster), attr))
            });
            match boot {
                1 => {
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    (id, initial)
                }
                _ => {
                    let id = previous.unwrap();
                    let changed = receive_observed(&client, id).await.unwrap();
                    wait_committed(&state).await;
                    (id, changed)
                }
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("frozen light boot {boot} timed out");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    (id, observed)
}

#[test]
fn frozen_light_report_action_carries_root_bdbi_and_changed_bounds() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let (subscription, before) = frozen_light_boot(directory.path(), 1, None);
            assert_eq!(before.roots[&0].1, 1);
            assert_eq!(before.bdbis[&3].1, 1);
            assert_eq!(before.lighting[&(2, 0x300, 0x400b)].1, 154);
            assert_eq!(before.lighting[&(2, 0x300, 0x400c)].1, 370);

            let (_, after) = frozen_light_boot(directory.path(), 2, Some(subscription));
            // One real ReportDataAction (fragments permitted) carries the
            // supplied root, frozen endpoint version and changed effective
            // bounds coherently through the protocol.
            assert_eq!(after.roots[&0].1, 2);
            assert_eq!(after.bdbis[&3].1, 2);
            assert_eq!(after.lighting[&(2, 0x300, 0x400b)].1, 250);
            assert_eq!(after.lighting[&(2, 0x300, 0x400c)].1, 333);
        })
        .unwrap()
        .join()
        .unwrap();
}
