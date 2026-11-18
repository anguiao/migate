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
    if boot == 4 {
        matter.with_state(|state| {
            let mut limited = AclEntry::new(None, Privilege::VIEW, AuthMode::Case);
            limited.add_subject(CLIENT_ID).unwrap();
            limited
                .add_target(rs_matter::acl::Target::new(Some(2), None, None))
                .unwrap();
            let fabric = state
                .fabrics
                .fabric_mut(NonZeroU8::new(1).unwrap())
                .unwrap();
            fabric.acl_update(0, limited).unwrap();
        });
    }
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
        let mut multi_entry = entry(&multi, 4, &[("outlet_1", 5), ("outlet_2", 6), ("usb", 7)]);
        multi_entry.identity.configuration_version = boot as u32;
        entries.push(multi_entry);
    }
    let handler = model_handler(&matter, entries);
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
                    assert_eq!(first.parts[&0], [1, 2, 3, 4, 5, 6, 7]);
                    assert_eq!(first.parts[&1], [2, 3, 4, 5, 6, 7]);
                    paths.insert(
                        0,
                        AttrPath::from_gp(&GenericPath::new(None, Some(6), Some(0))),
                    );
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(5),
                        Some(6),
                        Some(0),
                    )));
                    paths.push(AttrPath::from_gp(&GenericPath::new(
                        Some(3),
                        Some(0x39),
                        Some(24),
                    )));
                    let (id, baseline) = subscribe_paths(&client, &paths).await.unwrap();
                    wait_committed(&state).await;
                    assert_eq!(
                        baseline.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 5, 6, 7]
                    );
                    assert!(baseline.powers.values().all(|(_, power)| !power));
                    assert_eq!(baseline.bdbis[&3].1, 1);
                    multi.function("usb").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&7],
                        (baseline.powers[&7].0.wrapping_add(1), true)
                    );
                    assert!(!changed.powers.contains_key(&2));
                    wait_committed(&state).await;
                    id
                }
                2 => {
                    let id = previous.unwrap();
                    let restored = receive_observed(&client, id).await.unwrap();
                    assert_eq!(restored.status, [(5, IMStatusCode::UnsupportedEndpoint)]);
                    assert_eq!(restored.parts[&0], [1, 2, 3]);
                    assert_eq!(restored.parts[&1], [2, 3]);
                    assert_eq!(restored.powers.keys().copied().collect::<Vec<_>>(), [2]);
                    wait_committed(&state).await;
                    id
                }
                3 => {
                    let id = previous.unwrap();
                    let added = receive_observed(&client, id).await.unwrap();
                    assert_eq!(added.parts[&0], [1, 2, 3, 4, 5, 6, 7]);
                    assert_eq!(added.parts[&1], [2, 3, 4, 5, 6, 7]);
                    assert_eq!(
                        added.powers.keys().copied().collect::<Vec<_>>(),
                        [2, 5, 6, 7]
                    );
                    assert!(!added.powers[&2].1);
                    assert!(!added.powers[&5].1);
                    wait_committed(&state).await;
                    multi.function("outlet_1").unwrap().observe(true);
                    let changed = receive_observed(&client, id).await.unwrap();
                    assert_eq!(
                        changed.powers[&5],
                        (added.powers[&5].0.wrapping_add(1), true)
                    );
                    wait_committed(&state).await;
                    id
                }
                4 => {
                    let id = previous.unwrap();
                    let restored = receive_observed(&client, id).await.unwrap();
                    assert_eq!(restored.powers.keys().copied().collect::<Vec<_>>(), [2]);
                    assert!(restored.parts.is_empty() && restored.bdbis.is_empty());
                    assert_eq!(restored.status.len(), 4);
                    assert!(
                        restored
                            .status
                            .iter()
                            .all(|(_, status)| *status == IMStatusCode::UnsupportedAccess)
                    );
                    wait_committed(&state).await;
                    id
                }
                _ => unreachable!(),
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
fn restored_subscription_follows_current_endpoints_and_access() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let id = dynamic_boot(directory.path(), 1, None);
            let restored = dynamic_boot(directory.path(), 2, Some(id));
            assert_eq!(dynamic_boot(directory.path(), 3, Some(restored)), id);
            assert_eq!(dynamic_boot(directory.path(), 4, Some(id)), id);
        })
        .unwrap()
        .join()
        .unwrap();
}
