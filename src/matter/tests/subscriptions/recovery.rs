use super::*;

fn recovery_race_boot(directory: &Path, previous_subscription: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    if previous_subscription.is_none() {
        connect(&matter, SERVER_ID, CLIENT_ID, 41);
        connect(&client, CLIENT_ID, SERVER_ID, 41);
    }
    if previous_subscription.is_none() {
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
    let (device, _receiver) = super::super::test_device();
    device.set_availability(Availability::Available);
    device.functions()[0].observe(false);
    let handler = crate::matter::tests::support::model_handler(vec![
        crate::matter::tests::support::single_entry(&device),
    ]);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let id = block_on(async {
        im.startup().await.unwrap();
        if previous_subscription.is_some() {
            assert!(matter.has_fabrics());
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
            panic!("Matter services exited during the session recovery race");
        };
        let controller = async {
            if previous_subscription.is_some() {
                // Wait until the restored report is resolving the peer
                // before the new inbound CASE session arrives.
                let service = matter.transport().wait_mdns_resolve_request().await;
                assert!(matches!(
                    service,
                    rs_matter::transport::network::MatterRemoteService::Operational {
                        node_id: CLIENT_ID,
                        ..
                    }
                ));
                assert!(matter.transport().mdns_resolve_in_flight());
                connect(&matter, SERVER_ID, CLIENT_ID, 42);
                connect(&client, CLIENT_ID, SERVER_ID, 42);
                // Hold an idle exchange bound to this exact session so that
                // its public API can verify liveness without another lookup.
                let session_probe = Exchange::initiate(
                    &matter,
                    test_only_crypto(),
                    NonZeroU8::new(1).unwrap(),
                    CLIENT_ID,
                )
                .await
                .unwrap();
                // No resolver answer is deposited; the real resolve timer
                // produces the failure while the inbound session exists.
                assert!(matter.transport().mdns_resolve_in_flight());
                // Synchronize with the actual retirement instead of a time delay.
                // Upstream gives up on a restore whose resolve failed without
                // signalling `wait_report_failed`, so wait for the old
                // subscription to disappear.
                while state
                    .subscriptions()
                    .has_subscription_for(NonZeroU8::new(1).unwrap(), CLIENT_ID)
                {
                    futures_lite::future::yield_now().await;
                }
                assert!(
                    session_probe.pending_retrans().is_ok(),
                    "a failed restored-subscription operation removed the new inbound CASE session"
                );
                // A replacement subscription is established over the same
                // inbound session that the race already bound; no reconnect.
                let replacement = subscribe(&client, false).await.unwrap();
                wait_committed(&state).await;
                for power in [true, false] {
                    device.functions()[0].observe(power);
                    expect_report(&client, replacement, Some(power), None)
                        .await
                        .unwrap();
                    wait_committed(&state).await;
                }
                replacement
            } else {
                let id = subscribe(&client, false).await.unwrap();
                wait_committed(&state).await;
                id
            }
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(15)).await;
                panic!("timed out waiting for the session recovery race");
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
fn resumed_resolve_failure_must_preserve_new_inbound_session() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let previous = recovery_race_boot(directory.path(), None);
            // Upstream retires a failed restore and the replacement subscription
            // gets its own ID, so no relationship with `previous` is required.
            recovery_race_boot(directory.path(), Some(previous));
        })
        .unwrap()
        .join()
        .unwrap();
}
