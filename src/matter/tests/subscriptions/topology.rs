use super::*;

fn dynamic_boot(directory: &Path, boot: u16, previous: Option<u32>) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let info = basic_info(&identity);
    let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = StoreAdapter::new(store.matter());
    let kv = matter.kv(protocol.clone());
    matter.startup(&kv).unwrap();
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&matter, SERVER_ID, CLIENT_ID, boot + 20);
    connect(&client, CLIENT_ID, SERVER_ID, boot + 20);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState = InteractionModelState::new();
    let (single, _single_receivers) = super::super::physical("dynamic-single", false);
    let (multi, _multi_receivers) = super::super::physical("dynamic-multi", true);
    for device in [&single, &multi] {
        device.set_availability(Availability::Available);
        for function in device.functions() {
            function.observe(false);
        }
    }
    // Supply each frozen topology directly. Only Matter subscription state is persisted.
    let mut entries = vec![single_entry(&single)];
    if boot != 2 {
        let mut multi_entry = entry(&multi, 3, &[("outlet_1", 4), ("outlet_2", 5), ("usb", 6)]);
        multi_entry.identity.configuration_version = boot as u32;
        entries.push(multi_entry);
    }
    let handler = model_handler(entries);
    let im = InteractionModel::new(&matter, &crypto, &buffers, &handler, &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);
    let id = block_on(async {
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
            panic!("dynamic Matter services exited");
        };
        let controller = async {
            let mut paths = vec![
                AttrPath::from_gp(&GenericPath::new(Some(0), Some(0x1d), Some(3))),
                AttrPath::from_gp(&GenericPath::new(Some(1), Some(0x1d), Some(3))),
            ];
            let id = match boot {
                1 => {
                    let (_, first) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert!(first.powers.is_empty());
                    assert_eq!(first.parts[&0], [1, 2, 3, 4, 5, 6]);
                    assert_eq!(first.parts[&1], [2, 3, 4, 5, 6]);
                    paths.insert(
                        0,
                        AttrPath::from_gp(&GenericPath::new(None, Some(6), Some(0))),
                    );
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(4),
                        Some(6),
                        Some(0),
                    )));
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(2),
                        Some(0x39),
                        Some(24),
                    )));
                    let (id, baseline) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        baseline.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 4, 5, 6]
                    );
                    assert!(baseline.powers.values().all(|(_, power)| !power));
                    assert_eq!(baseline.bdbis[&2].1, 1);
                    multi.function("usb").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&6],
                        (baseline.powers[&6].0.wrapping_add(1), true)
                    );
                    assert!(!changed.powers.contains_key(&2));
                    wait_committed(&state).await;
                    id
                }
                2 => {
                    let id = previous.unwrap();
                    let restored = receive_observed(&client, id).await.unwrap();
                    assert_eq!(restored.unsupported, [4]);
                    assert_eq!(restored.parts[&0], [1, 2]);
                    assert_eq!(restored.parts[&1], [2]);
                    assert_eq!(restored.powers.keys().copied().collect::<Vec<_>>(), [2]);
                    wait_committed(&state).await;
                    id
                }
                _ => {
                    let id = previous.unwrap();
                    let added = receive_observed(&client, id).await.unwrap();
                    assert_eq!(added.parts[&0], [1, 2, 3, 4, 5, 6]);
                    assert_eq!(added.parts[&1], [2, 3, 4, 5, 6]);
                    assert_eq!(
                        added.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 4, 5, 6]
                    );
                    assert!(!added.powers[&2].1);
                    assert!(!added.powers[&4].1);
                    wait_committed(&state).await;
                    multi.function("outlet_1").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&4],
                        (added.powers[&4].0.wrapping_add(1), true)
                    );
                    wait_committed(&state).await;
                    id
                }
            };
            single.functions()[0].observe(true);
            let changed = receive_observed(&client, id).await.unwrap();
            assert!(changed.powers[&2].1);
            wait_committed(&state).await;
            id
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("dynamic subscription boot {boot} timed out");
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
fn dynamic_subscription_reports_add_delete_and_restores_deleted_paths_after_restart() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let id = dynamic_boot(directory.path(), 1, None);
            let restored = dynamic_boot(directory.path(), 2, Some(id));
            assert_eq!(dynamic_boot(directory.path(), 3, Some(restored)), id);
        })
        .unwrap()
        .join()
        .unwrap();
}
