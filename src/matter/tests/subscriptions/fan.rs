use super::super::support::{
    Context, fan_device, observe_fan_state, read_bytes, report_dataver, report_value,
};
use super::*;
use crate::device::{DeviceRequest, FanCommand, FanStep, FanValue};
async fn no_report(client: &Matter<'_>, id: u32) {
    or(
        async {
            let report = receive_observed(client, id).await.unwrap();
            panic!("unexpected fan report: {report:?}");
        },
        async { async_io::Timer::after(Duration::from_millis(200)).await },
    )
    .await;
}

fn values(report: &Observed) -> Vec<(u32, u8)> {
    report
        .fan
        .iter()
        .map(|(id, (_, value))| (*id, *value))
        .collect()
}

#[test]
fn fan_live_subscription_reports_only_changed_projections() {
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
            connect(&matter, SERVER_ID, CLIENT_ID, 82);
            connect(&client, CLIENT_ID, SERVER_ID, 82);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, mut receiver) = fan_device("subscription-fan", true);
            device.set_availability(Availability::Available);
            let function = &device.functions()[0];
            observe_fan_state(function, true, 40, 0, false);
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
                    panic!("fan services exited");
                };
                let controller = async {
                    let mut paths = [0, 2, 3, 5, 6, 8]
                        .map(|a| {
                            AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x202), Some(a)))
                        })
                        .to_vec();
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(2),
                        Some(6),
                        Some(0),
                    )));
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert!(initial.powers[&2].1);
                    assert_eq!(
                        values(&initial),
                        [(0, 2), (2, 40), (3, 40), (5, 40), (6, 40), (8, 0)]
                    );
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );

                    function.observe_fan(FanValue::Speed(41), Some(2), 2);
                    let speed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(values(&speed), [(2, 41), (3, 41), (5, 41), (6, 41)]);
                    assert!(speed.powers.is_empty());
                    assert_ne!(speed.fan[&2].0, initial.fan[&2].0);
                    wait_committed(&state).await;
                    function.observe_fan(FanValue::Speed(41), Some(3), 3);
                    function.observe_fan(FanValue::Swing(false), Some(3), 3);
                    no_report(&client, id).await;

                    for (attribute, bytes, expected) in [
                        (0, &[0x04, 7][..], IMStatusCode::ConstraintError),
                        (0, &[0x04, 255][..], IMStatusCode::ConstraintError),
                        (2, &[0x14][..], IMStatusCode::InvalidInState),
                        (5, &[0x04, 101][..], IMStatusCode::ConstraintError),
                        (8, &[0x04, 0x80][..], IMStatusCode::ConstraintError),
                    ] {
                        assert_eq!(
                            write_status(&client, 0x202, attribute, bytes).await,
                            expected
                        );
                        assert!(
                            futures_lite::future::poll_once(receiver.next_operation())
                                .await
                                .is_none()
                        );
                    }
                    let action = function.begin_fan(FanCommand::Swing(true)).unwrap();
                    let DeviceRequest::Fan(request) = receiver.next_operation().await.unwrap()
                    else {
                        panic!("expected fan request")
                    };
                    request.mark_sent(FanStep::Swing(true)).unwrap();
                    request.step_accepted().unwrap();
                    request.accept().unwrap();
                    action.accepted().await.unwrap();
                    no_report(&client, id).await;
                    function.observe_fan(FanValue::Swing(true), Some(4), 4);
                    let swing = receive_observed(&client, id).await.unwrap();
                    assert_eq!(values(&swing), [(8, 1)]);
                    assert!(swing.powers.is_empty());
                    assert_ne!(swing.fan[&8].0, speed.fan[&2].0);
                    wait_committed(&state).await;

                    function.observe_fan(FanValue::Speed(67), Some(5), 5);
                    let high = receive_observed(&client, id).await.unwrap();
                    assert_eq!(values(&high), [(0, 3), (2, 67), (3, 67), (5, 67), (6, 67)]);
                    assert!(high.powers.is_empty());
                    wait_committed(&state).await;
                    function.observe_fan(FanValue::Power(false), Some(6), 6);
                    let off = receive_observed(&client, id).await.unwrap();
                    assert!(!off.powers[&2].1);
                    assert_ne!(off.powers[&2].0, initial.powers[&2].0);
                    assert_eq!(values(&off), [(0, 0), (2, 0), (3, 0), (5, 0), (6, 0)]);
                    assert_ne!(off.fan[&0].0, high.fan[&0].0);
                    wait_committed(&state).await;
                    function.observe_fan(FanValue::Speed(80), Some(7), 7);
                    no_report(&client, id).await;
                    assert_eq!(function.snapshot().fan().unwrap().speed.value, Some(80));

                    function.observe_fan(FanValue::Power(true), Some(8), 8);
                    let restored = receive_observed(&client, id).await.unwrap();
                    assert!(restored.powers[&2].1);
                    assert_ne!(restored.powers[&2].0, off.powers[&2].0);
                    assert_eq!(
                        values(&restored),
                        [(0, 3), (2, 80), (3, 80), (5, 80), (6, 80)]
                    );
                    assert_ne!(restored.fan[&0].0, off.fan[&0].0);
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(8)).await;
                        panic!("fan subscription timed out");
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
fn locked_stack_fan_write_report_preserves_observations_and_concurrent_feedback() {
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
            connect(&matter, SERVER_ID, CLIENT_ID, 83);
            connect(&client, CLIENT_ID, SERVER_ID, 83);
            let crypto = test_only_crypto();
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: InteractionModelState = InteractionModelState::new();
            let (device, mut receiver) = fan_device("fan-write-report", true);
            device.set_availability(Availability::Available);
            let function = &device.functions()[0];
            observe_fan_state(function, true, 40, 1, false);
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
                    panic!("fan write services exited");
                };
                let controller = async {
                    let paths = [2, 8].map(|attribute| {
                        AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x202), Some(attribute)))
                    });
                    let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(values(&initial), [(2, 40), (8, 0)]);
                    let observed = function.snapshot().fan().unwrap();
                    let accept = async {
                        let DeviceRequest::Fan(request) = receiver.next_operation().await.unwrap()
                        else {
                            panic!("expected fan request")
                        };
                        assert_eq!(request.command, FanCommand::Swing(true));
                        request.mark_sent(FanStep::Swing(true)).unwrap();
                        request.step_accepted().unwrap();
                        request.accept().unwrap();
                    };
                    let (status, ()) = futures_lite::future::zip(
                        write_status(&client, 0x202, 8, &[0x04, 1]),
                        accept,
                    )
                    .await;
                    assert_eq!(status, IMStatusCode::Success);
                    assert_eq!(function.snapshot().fan().unwrap(), observed);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );

                    // Exercise the retained-value report generated by the locked
                    // stack; individual writes are not required to produce one.
                    let retained = receive_observed(&client, id).await.unwrap();
                    assert_eq!(retained.fan[&8].1, 0);
                    assert_eq!(function.snapshot().fan().unwrap(), observed);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );
                    wait_committed(&state).await;
                    function.observe_fan(FanValue::Swing(true), Some(2), 2);
                    loop {
                        let report = receive_observed(&client, id).await.unwrap();
                        assert!(
                            futures_lite::future::poll_once(receiver.next_operation())
                                .await
                                .is_none()
                        );
                        if let Some((_, speed)) = report.fan.get(&2) {
                            assert_eq!(*speed, 40);
                        }
                        if let Some((version, swing)) = report.fan.get(&8) {
                            if *swing == 1 {
                                assert_ne!(*version, retained.fan[&8].0);
                                break;
                            }
                            assert_eq!(*swing, 0);
                        }
                    }
                    let actual_observed = function.snapshot().fan().unwrap();
                    assert_eq!(actual_observed.swing.value, Some(true));
                    assert_eq!(actual_observed.power, observed.power);
                    assert_eq!(actual_observed.speed, observed.speed);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );
                    wait_committed(&state).await;

                    let observed = function.snapshot().fan().unwrap();
                    let accept = async {
                        let DeviceRequest::Fan(request) = receiver.next_operation().await.unwrap()
                        else {
                            panic!("expected fan request")
                        };
                        assert_eq!(request.command, FanCommand::Speed(100));
                        assert!(!request.needs_power_on());
                        request.mark_sent(FanStep::Auxiliary).unwrap();
                        request.step_accepted().unwrap();
                        request.mark_sent(FanStep::Speed(100)).unwrap();
                        request.step_accepted().unwrap();
                        request.accept().unwrap();
                    };
                    let (status, ()) = futures_lite::future::zip(
                        write_status(&client, 0x202, 2, &[0x04, 100]),
                        accept,
                    )
                    .await;
                    assert_eq!(status, IMStatusCode::Success);
                    assert_eq!(function.snapshot().fan().unwrap(), observed);
                    assert!(
                        futures_lite::future::poll_once(receiver.next_operation())
                            .await
                            .is_none()
                    );
                    let read = Context::new(&im, 0x202, 2);
                    let before_feedback = read_bytes(&handler, &read).await.unwrap();
                    assert_eq!(report_value(&before_feedback).u8().unwrap(), 40);
                    let version_before_feedback = report_dataver(&before_feedback);
                    assert_eq!(function.snapshot().fan().unwrap(), observed);

                    let mut pending = Exchange::accept(&client).await.unwrap();
                    pending.recv_fetch().await.unwrap();
                    {
                        let rx = pending.rx().unwrap();
                        assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
                        let report =
                            ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                        assert_eq!(report.subscription_id, Some(id));
                        assert!(!report.more_chunks.unwrap_or(false));
                        let mut retained = Observed::default();
                        retained.add(&report);
                        assert_eq!(retained.fan[&2], (version_before_feedback, 40));
                    }

                    // Publish new feedback while the retained report still awaits
                    // its status response. Committing it must not consume this change.
                    function.observe_fan(FanValue::Speed(100), Some(3), 3);
                    futures_lite::future::yield_now().await;
                    pending
                        .send_with(|_, buffer| {
                            StatusResp::write(buffer, IMStatusCode::Success)?;
                            Ok(Some(OpCode::StatusResponse.into()))
                        })
                        .await
                        .unwrap();
                    pending.acknowledge().await.unwrap();
                    drop(pending);
                    loop {
                        let report = receive_observed(&client, id).await.unwrap();
                        assert!(
                            futures_lite::future::poll_once(receiver.next_operation())
                                .await
                                .is_none()
                        );
                        if let Some((_, swing)) = report.fan.get(&8) {
                            assert_eq!(*swing, 1);
                        }
                        if let Some((version, speed)) = report.fan.get(&2) {
                            if *speed == 100 {
                                assert_ne!(*version, version_before_feedback);
                                break;
                            }
                            assert_eq!(*speed, 40);
                        }
                    }
                    let actual_observed = function.snapshot().fan().unwrap();
                    assert_eq!(actual_observed.speed.value, Some(100));
                    assert_eq!(actual_observed.power, observed.power);
                    assert_eq!(actual_observed.swing, observed.swing);
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(8)).await;
                        panic!("fan write subscription timed out");
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
                .unwrap();
        });
        fabrics.run().unwrap();
    }
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let (device, mut receiver) = fan_device("persistent-fan", true);
    device.set_availability(Availability::Available);
    let function = &device.functions()[0];
    observe_fan_state(
        function,
        previous.is_none(),
        if previous.is_some() { 77 } else { 40 },
        1,
        previous.is_some(),
    );
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
            panic!("fan recovery services exited");
        };
        let controller = async {
            assert!(
                futures_lite::future::poll_once(receiver.next_operation())
                    .await
                    .is_none()
            );
            if let Some(id) = previous {
                let restored = receive_observed(&client, id).await.unwrap();
                assert!(!restored.powers[&2].1);
                assert_eq!(
                    values(&restored),
                    [(0, 0), (2, 0), (3, 0), (5, 0), (6, 0), (8, 1)]
                );
                assert!(
                    futures_lite::future::poll_once(receiver.next_operation())
                        .await
                        .is_none()
                );
                wait_committed(&state).await;
                function.observe_fan(FanValue::Power(true), Some(2), 2);
                let resumed = receive_observed(&client, id).await.unwrap();
                assert!(resumed.powers[&2].1);
                assert_eq!(
                    values(&resumed),
                    [(0, 3), (2, 77), (3, 77), (5, 77), (6, 77)]
                );
                assert!(
                    futures_lite::future::poll_once(receiver.next_operation())
                        .await
                        .is_none()
                );
                id
            } else {
                let mut paths = [0, 2, 3, 5, 6, 8]
                    .map(|a| AttrPath::from_gp(&GenericPath::new(Some(2), Some(0x202), Some(a))))
                    .to_vec();
                paths.push(AttrPath::from_gp(&GenericPath::new(
                    Some(2),
                    Some(6),
                    Some(0),
                )));
                let (id, initial) = subscribe_paths(&client, &paths).await.unwrap();
                assert_eq!(
                    values(&initial),
                    [(0, 2), (2, 40), (3, 40), (5, 40), (6, 40), (8, 0)]
                );
                wait_committed(&state).await;
                let action = function.begin_fan(FanCommand::Speed(40)).unwrap();
                let DeviceRequest::Fan(request) = receiver.next_operation().await.unwrap() else {
                    panic!("expected fan request")
                };
                request.mark_sent(FanStep::Auxiliary).unwrap();
                request.step_accepted().unwrap();
                request.mark_sent(FanStep::Speed(40)).unwrap();
                request.step_accepted().unwrap();
                request.accept().unwrap();
                action.accepted().await.unwrap();
                no_report(&client, id).await;
                id
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(8)).await;
                panic!("fan recovery subscription timed out");
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
fn persisted_fan_subscription_restores_observations_without_replaying_commands() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let identity = Store::open(directory.path())
                .unwrap()
                .load_identity()
                .unwrap();
            let id = persistent_boot(directory.path(), 91, None);
            persistent_boot(directory.path(), 92, Some(id));
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
