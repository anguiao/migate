use super::super::support::{Context, read_bytes, report_dataver, report_value};
use super::*;
use crate::device::{
    CurtainCommand, CurtainMotion, CurtainValue, DeviceKey, DeviceRequest, FunctionDescription,
    FunctionKind, PhysicalDevice, PhysicalDeviceDescription,
};

#[test]
fn curtain_live_subscription_handles_noop_write_and_actual_observations() {
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
            connect(&matter, SERVER_ID, CLIENT_ID, 81);
            connect(&client, CLIENT_ID, SERVER_ID, 81);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, mut receivers) = PhysicalDevice::new(
                PhysicalDeviceDescription::new(
                    DeviceKey::new("xiaomi", "subscription-curtain"),
                    "Curtain",
                    vec![FunctionDescription {
                        id: "main".into(),
                        label: "Curtain".into(),
                        kind: FunctionKind::Curtain,
                    }],
                )
                .unwrap(),
            );
            device.set_availability(Availability::Available);
            let function = &device.functions()[0];
            function.observe_curtain(CurtainValue::CurrentPosition(20), Some(1), 1);
            function.observe_curtain(CurtainValue::TargetPosition(80), Some(1), 1);
            function.observe_curtain(CurtainValue::Status(CurtainMotion::Opening), Some(1), 1);
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
                    panic!("curtain services exited");
                };
                let controller = async {
                    let paths = [8, 10, 11, 14, 23].map(|a| {
                        AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x102), Some(a)))
                    });
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        initial
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [
                            (8, Some(80)),
                            (10, Some(5)),
                            (11, Some(2000)),
                            (14, Some(8000)),
                            (23, Some(0))
                        ]
                    );
                    let observed = function.snapshot().curtain().unwrap();
                    assert_eq!(
                        write_status(&client, 0x102, 23, &[0x04, 0]).await,
                        IMStatusCode::Success
                    );
                    assert_eq!(function.snapshot().curtain().unwrap(), observed);
                    assert!(
                        futures_lite::future::poll_once(receivers[0].next_operation())
                            .await
                            .is_none()
                    );
                    let mode = Context::new(&im, 0x102, 23);
                    let position = Context::new(&im, 0x102, 8);
                    assert_eq!(
                        report_value(&read_bytes(&handler, &mode).await.unwrap())
                            .u8()
                            .unwrap(),
                        0
                    );
                    let before_feedback = read_bytes(&handler, &position).await.unwrap();
                    assert_eq!(report_value(&before_feedback).u16().unwrap(), 80);
                    let version_before_feedback = report_dataver(&before_feedback);
                    assert_eq!(function.snapshot().curtain().unwrap(), observed);
                    function.observe_curtain(CurtainValue::CurrentPosition(25), Some(2), 2);
                    let mut current_version = None;
                    let mut current_100ths = false;
                    while current_version.is_none() || !current_100ths {
                        let report = receive_observed(&client, id).await.unwrap();
                        assert!(
                            futures_lite::future::poll_once(receivers[0].next_operation())
                                .await
                                .is_none()
                        );
                        for (attribute, (version, value)) in report.curtain {
                            match (attribute, value) {
                                (8, Some(75)) => current_version = Some(version),
                                (14, Some(7500)) => current_100ths = true,
                                (8, Some(80)) | (14, Some(8000)) | (23, Some(0)) => {}
                                other => panic!("unexpected curtain report: {other:?}"),
                            }
                        }
                    }
                    assert_ne!(current_version.unwrap(), version_before_feedback);
                    let actual = function.snapshot().curtain().unwrap();
                    assert_eq!(actual.current_position.value, Some(25));
                    assert_eq!(actual.target_position, observed.target_position);
                    assert_eq!(actual.status, observed.status);
                    wait_committed(&state).await;
                    function.observe_curtain(CurtainValue::TargetPosition(60), Some(3), 3);
                    let target_version = loop {
                        let report = receive_observed(&client, id).await.unwrap();
                        assert!(
                            futures_lite::future::poll_once(receivers[0].next_operation())
                                .await
                                .is_none()
                        );
                        let mut version = None;
                        for (attribute, (reported_version, value)) in report.curtain {
                            match (attribute, value) {
                                (11, Some(4000)) => version = Some(reported_version),
                                (8, Some(75)) | (14, Some(7500)) | (23, Some(0)) => {}
                                other => panic!("unexpected curtain report: {other:?}"),
                            }
                        }
                        if let Some(version) = version {
                            break version;
                        }
                    };
                    wait_committed(&state).await;
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Closing),
                        Some(4),
                        4,
                    );
                    let status = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        status
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [(10, Some(10))]
                    );
                    wait_committed(&state).await;
                    function.observe_curtain(CurtainValue::CurrentPosition(25), Some(5), 5);
                    function.observe_curtain(CurtainValue::TargetPosition(60), Some(5), 5);
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Closing),
                        Some(5),
                        5,
                    );
                    or(
                        async {
                            let report = receive_observed(&client, id).await.unwrap();
                            panic!("unexpected duplicate report: {report:?}");
                        },
                        async { async_io::Timer::after(Duration::from_millis(200)).await },
                    )
                    .await;

                    // Both Stops are queued before the first is sent. Its real
                    // feedback still publishes the converged target immediately.
                    let first = function.begin_curtain(CurtainCommand::Stop).unwrap();
                    let second = function.begin_curtain(CurtainCommand::Stop).unwrap();
                    let DeviceRequest::Curtain(request) =
                        receivers[0].next_operation().await.unwrap()
                    else {
                        panic!("expected Stop")
                    };
                    assert_eq!(request.command, CurtainCommand::Stop);
                    request.mark_sent().unwrap();
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Stopped),
                        Some(5),
                        5,
                    );
                    let stopped = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        stopped
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [(10, Some(0)), (11, Some(7500))]
                    );
                    assert_ne!(stopped.curtain[&11].0, target_version);
                    request.accept().unwrap();
                    first.accepted().await.unwrap();
                    let DeviceRequest::Curtain(request) =
                        receivers[0].next_operation().await.unwrap()
                    else {
                        panic!("expected second Stop")
                    };
                    assert_eq!(request.command, CurtainCommand::Stop);
                    request.mark_sent().unwrap();
                    request.accept().unwrap();
                    second.accepted().await.unwrap();
                    wait_committed(&state).await;
                    function.observe_curtain(CurtainValue::CurrentPosition(25), Some(5), 5);
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Stopped),
                        Some(5),
                        5,
                    );
                    or(
                        async {
                            let report = receive_observed(&client, id).await.unwrap();
                            panic!("unexpected duplicate stop report: {report:?}");
                        },
                        async { async_io::Timer::after(Duration::from_millis(200)).await },
                    )
                    .await;
                    // A new movement followed by an external stop must publish
                    // the same settled target without a local Stop request.
                    wait_committed(&state).await;
                    function.observe_curtain(CurtainValue::TargetPosition(91), Some(6), 6);
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Opening),
                        Some(6),
                        6,
                    );
                    let resumed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        resumed
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [(10, Some(5)), (11, Some(900))]
                    );
                    wait_committed(&state).await;
                    function.observe_curtain(
                        CurtainValue::Status(CurtainMotion::Stopped),
                        Some(7),
                        7,
                    );
                    let stopped = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        stopped
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [(10, Some(0)), (11, Some(7500))]
                    );
                    wait_committed(&state).await;
                    function.observe_curtain(CurtainValue::CurrentPosition(57), Some(8), 8);
                    let final_position = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        final_position
                            .curtain
                            .iter()
                            .map(|(id, (_, value))| (*id, *value))
                            .collect::<Vec<_>>(),
                        [(8, Some(43)), (11, Some(4300)), (14, Some(4300))]
                    );
                    assert_eq!(
                        function.snapshot().curtain().unwrap().target_position.value,
                        Some(91)
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(8)).await;
                        panic!("curtain subscription timed out");
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
