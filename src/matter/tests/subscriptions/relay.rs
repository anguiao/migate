use super::*;
use crate::matter::tests::support::{Context, read_bytes, report_dataver, report_value};
use crate::{
    configuration::Scope,
    device::{
        DeviceKey, FunctionDescription, FunctionKind, PhysicalDevice, PhysicalDeviceDescription,
    },
    matter::PublishedRelay,
};
use rs_matter::dm::{AsyncHandler, LifecycleOp, clusters::decl::binding::BindingAttrWrites as _};

#[test]
fn relay_subscriptions_report_only_the_changed_physical_channel() {
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
            let (device, _receivers) = PhysicalDevice::new(
                PhysicalDeviceDescription::new(
                    DeviceKey::new("xiaomi", "panel"),
                    "Panel",
                    (2..=4)
                        .map(|siid| FunctionDescription {
                            id: format!("relay:{siid}"),
                            label: format!("Relay {siid}"),
                            kind: FunctionKind::Relay,
                        })
                        .collect(),
                )
                .unwrap(),
            );
            device.set_availability(Availability::Available);
            for function in device.functions() {
                function.observe(false);
            }
            let scope = Scope {
                uid: "account".into(),
                home_id: "home".into(),
            };
            let entries: Vec<_> = (2..=4)
                .map(|siid| {
                    let mut entry = entry(&device, siid, &[]);
                    entry.identity.relay = Some(PublishedRelay {
                        scope: scope.clone(),
                        function_id: format!("relay:{siid}"),
                    });
                    entry
                })
                .collect();
            let handler = model_handler(entries.clone());
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
                    panic!("Matter services exited");
                };
                let controller = async {
                    let paths = [
                        AttrPath::from_gp(&GenericPath::new(None, Some(6), Some(0))),
                        AttrPath::from_gp(&GenericPath::new(Some(1), Some(0x1d), Some(3))),
                    ];
                    let (id, first) = subscribe_paths(&client, &paths).await.unwrap();
                    assert_eq!(first.parts[&1], [2, 3, 4]);
                    assert_eq!(first.powers.keys().copied().collect::<Vec<_>>(), [2, 3, 4]);
                    assert!(first.powers.values().all(|(_, value)| !value));
                    wait_committed(&state).await;
                    let before_two = read_bytes(&handler, &Context::at(&im, 2, 0x1e, 0))
                        .await
                        .unwrap();
                    let before_three = read_bytes(&handler, &Context::at(&im, 3, 0x1e, 0))
                        .await
                        .unwrap();
                    let exchange =
                        Exchange::initiate(&client, &crypto, NonZeroU8::new(1).unwrap(), SERVER_ID)
                            .await
                            .unwrap();
                    let written = exchange
                        .write_with(None, |builder| {
                            let requests = builder.write_requests()?;
                            let bindings = requests.binding_write().binding(3)?;
                            bindings
                                .push()?
                                .node(Some(1234))?
                                .group(None)?
                                .endpoint(Some(8))?
                                .cluster(Some(6))?
                                .fabric_index(None)?
                                .end()?
                                .end()?
                                .end()?
                                .end()?
                                .end()
                        })
                        .await
                        .unwrap();
                    for status in written.response().unwrap().write_responses.iter() {
                        assert_eq!(status.unwrap().status.status, IMStatusCode::Success);
                    }
                    drop(written);
                    let after_two = read_bytes(&handler, &Context::at(&im, 2, 0x1e, 0))
                        .await
                        .unwrap();
                    let after_three = read_bytes(&handler, &Context::at(&im, 3, 0x1e, 0))
                        .await
                        .unwrap();
                    assert_eq!(report_dataver(&after_two), report_dataver(&before_two));
                    assert_eq!(
                        report_dataver(&after_three),
                        report_dataver(&before_three).wrapping_add(1)
                    );
                    assert!(
                        store
                            .matter()
                            .contains(rs_matter::persist::BINDINGS_KEY)
                            .unwrap()
                    );
                    let restored = model_handler(entries.clone());
                    restored.lifecycle(&im, LifecycleOp::Startup).unwrap();
                    let bytes = read_bytes(&restored, &Context::at(&im, 3, 0x1e, 0))
                        .await
                        .unwrap();
                    assert_eq!(report_value(&bytes).array().unwrap().iter().count(), 1);
                    let bytes = read_bytes(&restored, &Context::at(&im, 2, 0x1e, 0))
                        .await
                        .unwrap();
                    assert_eq!(report_value(&bytes).array().unwrap().iter().count(), 0);

                    // The protocol registry has one shared 16-entry capacity,
                    // not a separate allowance for each endpoint.
                    let exchange =
                        Exchange::initiate(&client, &crypto, NonZeroU8::new(1).unwrap(), SERVER_ID)
                            .await
                            .unwrap();
                    let filled = exchange
                        .write_with(None, |builder| {
                            let requests = builder.write_requests()?;
                            let mut bindings = requests.binding_write().binding(3)?;
                            for node in 1..=16 {
                                bindings = bindings
                                    .push()?
                                    .node(Some(node))?
                                    .group(None)?
                                    .endpoint(Some(8))?
                                    .cluster(Some(6))?
                                    .fabric_index(None)?
                                    .end()?;
                            }
                            bindings.end()?.end()?.end()?.end()
                        })
                        .await
                        .unwrap();
                    for status in filled.response().unwrap().write_responses.iter() {
                        assert_eq!(status.unwrap().status.status, IMStatusCode::Success);
                    }
                    drop(filled);
                    let exchange =
                        Exchange::initiate(&client, &crypto, NonZeroU8::new(1).unwrap(), SERVER_ID)
                            .await
                            .unwrap();
                    let overflow = exchange
                        .write_with(None, |builder| {
                            let requests = builder.write_requests()?;
                            let bindings = requests.binding_write().binding(2)?;
                            bindings
                                .push()?
                                .node(Some(17))?
                                .group(None)?
                                .endpoint(Some(8))?
                                .cluster(Some(6))?
                                .fabric_index(None)?
                                .end()?
                                .end()?
                                .end()?
                                .end()?
                                .end()
                        })
                        .await
                        .unwrap();
                    for status in overflow.response().unwrap().write_responses.iter() {
                        assert_eq!(
                            status.unwrap().status.status,
                            IMStatusCode::ResourceExhausted
                        );
                    }
                    drop(overflow);
                    let bytes = read_bytes(&handler, &Context::at(&im, 3, 0x1e, 0))
                        .await
                        .unwrap();
                    assert_eq!(report_value(&bytes).array().unwrap().iter().count(), 16);
                    let bytes = read_bytes(&handler, &Context::at(&im, 2, 0x1e, 0))
                        .await
                        .unwrap();
                    assert_eq!(report_value(&bytes).array().unwrap().iter().count(), 0);
                    device.function("relay:3").unwrap().observe(true);
                    let report = receive_observed(&client, id).await.unwrap();
                    assert_eq!(report.powers.keys().copied().collect::<Vec<_>>(), [3]);
                    assert_eq!(
                        report.powers[&3],
                        (first.powers[&3].0.wrapping_add(1), true)
                    );
                    wait_committed(&state).await;
                    device.function("relay:4").unwrap().observe(true);
                    let report = receive_observed(&client, id).await.unwrap();
                    assert_eq!(report.powers.keys().copied().collect::<Vec<_>>(), [4]);
                    assert_eq!(
                        report.powers[&4],
                        (first.powers[&4].0.wrapping_add(1), true)
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("relay subscription timed out");
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
