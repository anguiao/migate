use super::*;

fn run_boot(directory: &Path, boot: u16, previous_subscription: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    if boot > 1 {
        assert!(matter.has_fabrics());
    }
    connect(&matter, SERVER_ID, CLIENT_ID, boot);
    if boot == 1 {
        matter.with_state(|state| {
            state
                .fabrics
                .fabric_mut(NonZeroU8::new(1).unwrap())
                .unwrap()
                .groups_mut()
                .key_map_add(GroupKeyMapping {
                    group_id: 123,
                    group_key_set_id: 1,
                })
                .unwrap()
        });
    }
    connect(&client, CLIENT_ID, SERVER_ID, boot);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let (device, mut receiver) = super::super::test_device();
    let command_function = device.functions()[0].clone();
    let initial = boot == 3;
    let rejection = RefCell::new(None::<CommandError>);
    let accepted = Cell::new(0);
    let handler = crate::matter::tests::support::model_handler(vec![
        crate::matter::tests::support::single_entry(&device),
    ]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        let services = async {
            let adapter = async {
                loop {
                    let request = next_power(&mut receiver).await.unwrap();
                    if let Some(error) = rejection.borrow_mut().take() {
                        request.reject(error);
                    } else {
                        command_function.observe(request.target);
                        request.accept().unwrap();
                        accepted.set(accepted.get() + 1);
                    }
                }
            };
            or(
                adapter,
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
                ),
            )
            .await
            .unwrap();
            panic!("Matter services exited during subscription test");
        };
        let controller = async {
            if boot == 1 {
                group_request(&client, true).await;
            }
            if boot == 1 {
                read_state(&client, None, false, &[]).await;
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                invoke_status(&client, 99, true, IMStatusCode::UnsupportedCommand).await;
                invoke_status(&client, 1, false, IMStatusCode::InvalidCommand).await;
                assert_eq!(accepted.get(), 0);
                device.set_availability(Availability::Available);
                invoke_status(&client, 2, true, IMStatusCode::Failure).await;
                read_state(&client, None, true, &[true]).await;
                device.functions()[0].observe(false);
                *rejection.borrow_mut() = Some(CommandError::Rejected("denied".into()));
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                *rejection.borrow_mut() = Some(CommandError::Timeout);
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                *rejection.borrow_mut() =
                    Some(CommandError::SendFailed("transport stopped".into()));
                invoke_status(&client, 1, true, IMStatusCode::Failure).await;
                invoke_status(&client, 1, true, IMStatusCode::Success).await;
                read_state(&client, Some(true), true, &[true]).await;
                device.set_availability(Availability::Unavailable("offline".into()));
                read_state(&client, Some(true), false, &[true, false]).await;
                device.set_availability(Availability::Unavailable("another reason".into()));
                read_state(&client, Some(true), false, &[true, false]).await;
                device.set_availability(Availability::Available);
                read_state(&client, Some(true), true, &[true, false, true]).await;
                device.functions()[0].observe(initial);
            }
            let mut id = if let Some(id) = previous_subscription {
                // No new SubscribeRequest: resume under the original ID and
                // report a fresh source value. Real CASE is validated manually.
                expect_report(&client, id, None, Some(false)).await.unwrap();
                device.functions()[0].observe(initial);
                device.set_availability(Availability::Available);
                expect_report(&client, id, Some(initial), Some(true))
                    .await
                    .unwrap();
                id
            } else {
                let first = subscribe(&client, initial).await.unwrap();
                wait_committed(&state).await;
                let replacement = subscribe(&client, initial).await.unwrap();
                assert_ne!(first, replacement);
                replacement
            };
            if boot > 1 {
                group_request(&client, false).await;
            }
            for (command, power) in if initial {
                [("off", false), ("on", true)]
            } else {
                [("on", true), ("off", false)]
            } {
                wait_committed(&state).await;
                super::super::terminal_command(&device, command).await;
                expect_report(&client, id, Some(power), None).await.unwrap();
            }
            wait_committed(&state).await;
            if boot == 2 {
                // A new subscription must not reuse the restored ID.
                let replacement = subscribe(
                    &client,
                    device.functions()[0]
                        .snapshot()
                        .power()
                        .and_then(crate::device::Power::value)
                        .unwrap(),
                )
                .await
                .unwrap();
                assert!(replacement > id);
                id = replacement;
            }
            wait_committed(&state).await;
            let final_power = !device.functions()[0]
                .snapshot()
                .power()
                .and_then(crate::device::Power::value)
                .unwrap();
            super::super::terminal_command(&device, if final_power { "on" } else { "off" }).await;
            expect_report(&client, id, Some(final_power), None)
                .await
                .unwrap();
            id
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("timed out waiting for a subscription report on boot {boot}");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(rs_matter::persist::FABRIC_KEYS_START + 1)
            .unwrap()
    );
    assert!(
        Store::open(directory)
            .unwrap()
            .matter()
            .contains(PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    id
}

#[test]
fn test_fabric_groups_and_subscription_survive_restarts_and_report_terminal_changes() {
    env_logger::Builder::from_default_env().is_test(true).init();
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let identity = Store::open(directory.path())
                .unwrap()
                .load_identity()
                .unwrap();
            let mut subscription = None;
            for boot in 1..=3 {
                subscription = Some(run_boot(directory.path(), boot, subscription));
                assert_eq!(
                    Store::open(directory.path())
                        .unwrap()
                        .load_identity()
                        .unwrap(),
                    identity
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
