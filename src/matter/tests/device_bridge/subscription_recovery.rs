use super::*;

fn run_temperature_subscription_boot(
    directory: &Path,
    boot: u16,
    previous_subscription: Option<u32>,
) -> u32 {
    let store = Store::open(directory).unwrap();
    let identity = store.load_identity().unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::TemperatureSensor);
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
    service.publish(
        id.clone(),
        "Temperature",
        FeatureCapabilities(vec![Capability::Temperature(NumericRange {
            minimum: -20.0,
            maximum: 60.0,
            step: 0.1,
            unit: NumericUnit::Celsius,
        })]),
    );
    let model = DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
    let basic_info = crate::matter::common::basic_info(&identity);
    let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    let protocol = crate::matter::storage::StoreAdapter::new(store.matter());
    let kv = server.kv(protocol.clone());
    server.startup(&kv).unwrap();
    if !store
        .matter()
        .contains(rs_matter::persist::BASIC_INFO_KEY)
        .unwrap()
    {
        crate::matter::common::initialize_basic_info(&server, &kv, true).unwrap();
    }
    let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
    connect(&server, 123456, 445566, boot);
    connect(&client, 445566, 123456, boot);
    let crypto = test_only_crypto();
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
    let mut random = rand::rng();
    let handler = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&SysNetifs)
        .build(&mut random)
        .chain(|endpoint, _| endpoint != 0, &model);
    let im = InteractionModel::new(&server, &crypto, &buffers, (&model, &handler), &kv, &state);
    let incoming = Pipe::default();
    let outgoing = Pipe::default();
    let responder = DefaultResponder::new(&im);

    let subscription = block_on(async {
        im.startup().await.unwrap();
        let services = async {
            or(
                server.run(
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
            panic!("Matter services exited during cold-start subscription test");
        };
        let controller = async {
            let mut current = if let Some(previous) = previous_subscription {
                expect_temperature_report(&client, previous, None)
                    .await
                    .unwrap();
                previous
            } else {
                let first = subscribe_temperature(&client, endpoint).await.unwrap();
                while !state
                    .subscriptions()
                    .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                {
                    futures_lite::future::yield_now().await;
                }
                let replacement = subscribe_temperature(&client, endpoint).await.unwrap();
                assert_ne!(first, replacement);
                replacement
            };
            while !state
                .subscriptions()
                .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
            {
                futures_lite::future::yield_now().await;
            }
            if boot == 2 {
                let replacement = subscribe_temperature(&client, endpoint).await.unwrap();
                assert!(replacement > current);
                current = replacement;
            }
            while !state
                .subscriptions()
                .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
            {
                futures_lite::future::yield_now().await;
            }
            service.apply_report(StateReport::new(
                id,
                service.next_report_version(),
                StateSource::Lan,
                boot.into(),
                [(
                    Property::Temperature,
                    PropertyValue::Temperature(20.0 + f64::from(boot)),
                )],
            ));
            expect_temperature_report(
                &client,
                current,
                Some(((20.0 + f64::from(boot)) * 100.0) as i16),
            )
            .await
            .unwrap();
            current
        };
        or(
            services,
            or(controller, async {
                async_io::Timer::after(Duration::from_secs(5)).await;
                panic!("timed out on cold-start subscription boot {boot}");
            }),
        )
        .await
    });
    protocol.check_failure().unwrap();
    assert!(
        store
            .matter()
            .contains(rs_matter::persist::PERSISTENT_SUBSCRIPTIONS_START)
            .unwrap()
    );
    subscription
}

#[test]
fn replaced_subscription_survives_three_cold_starts_on_real_device_model() {
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
                subscription = Some(run_temperature_subscription_boot(
                    directory.path(),
                    boot,
                    subscription,
                ));
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

#[test]
fn shape_rebuild_restores_existing_subscription_on_the_new_model() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let id = feature(FeatureRole::TemperatureSensor);
                let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
                service.publish(
                    id.clone(),
                    "Temperature",
                    FeatureCapabilities(vec![
                        Capability::Temperature(NumericRange {
                            minimum: -20.0,
                            maximum: 60.0,
                            step: 0.1,
                            unit: NumericUnit::Celsius,
                        }),
                        Capability::Battery(NumericRange {
                            minimum: 0.0,
                            maximum: 100.0,
                            step: 1.0,
                            unit: NumericUnit::Percent,
                        }),
                    ]),
                );
                let basic_info = crate::matter::common::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let kv = server.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
                crate::matter::common::initialize_basic_info(&server, &kv, true).unwrap();
                connect(&server, 123456, 445566, 72);
                connect(&client, 445566, 123456, 72);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let incoming = Pipe::default();
                let outgoing = Pipe::default();
                let generations = Cell::new(0_u8);
                let protocol = async {
                    loop {
                        let model = DeviceBridgeModel::new(
                            service.clone(),
                            store.devices(),
                            store.matter(),
                        )
                        .unwrap();
                        let mut random = rand::rng();
                        let handler = endpoints::EthSysHandlerBuilder::new()
                            .netif_diag(&SysNetifs)
                            .build(&mut random)
                            .chain(|endpoint, _| endpoint != 0, &model);
                        let im = InteractionModel::new(
                            &server,
                            &crypto,
                            &buffers,
                            (&model, &handler),
                            &kv,
                            &state,
                        );
                        im.startup().await.unwrap();
                        generations.set(generations.get() + 1);
                        if generations.get() > 1 {
                            assert!(
                                state
                                    .subscriptions()
                                    .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566,)
                            );
                        }
                        let responder = DefaultResponder::new(&im);
                        let rebuilt = or(
                            async {
                                responder.run::<4, 4>().await.unwrap();
                                panic!("Matter responder stopped before rebuild")
                            },
                            or(
                                async {
                                    im.run().await.unwrap();
                                    panic!("Matter model stopped before rebuild")
                                },
                                async {
                                    model.rebuild_requested().await;
                                    true
                                },
                            ),
                        )
                        .await;
                        assert!(rebuilt);
                    }
                };
                let services = async {
                    or(
                        server.run(
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
                            protocol,
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("Matter service loop stopped before the controller completed");
                };
                let controller = async {
                    while generations.get() < 1 {
                        futures_lite::future::yield_now().await;
                    }
                    let subscription = subscribe_temperature(&client, endpoint).await.unwrap();
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    assert!(store.matter().contains(0x0800).unwrap());
                    service.publish(
                        id.clone(),
                        "Temperature",
                        FeatureCapabilities(vec![Capability::Temperature(NumericRange {
                            minimum: -20.0,
                            maximum: 60.0,
                            step: 0.1,
                            unit: NumericUnit::Celsius,
                        })]),
                    );
                    while generations.get() < 2 {
                        futures_lite::future::yield_now().await;
                    }
                    assert!(store.matter().contains(0x0800).unwrap());
                    let mut priming = Exchange::accept(&client).await.unwrap();
                    priming.recv_fetch().await.unwrap();
                    {
                        let rx = priming.rx().unwrap();
                        let report =
                            ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                        assert_eq!(report.subscription_id, Some(subscription));
                    }
                    priming
                        .send_with(|_, buffer| {
                            StatusResp::write(buffer, IMStatusCode::Success)?;
                            Ok(Some(OpCode::StatusResponse.into()))
                        })
                        .await
                        .unwrap();
                    priming.acknowledge().await.unwrap();
                    drop(priming);
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    service.apply_report(StateReport::new(
                        id,
                        service.next_report_version(),
                        StateSource::Lan,
                        1,
                        [(Property::Temperature, PropertyValue::Temperature(23.5))],
                    ));
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    let rx = exchange.rx().unwrap();
                    let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                    assert_eq!(report.subscription_id, Some(subscription));
                    assert_eq!(
                        report
                            .attrs::<Nullable<i16>>(
                                temperature_measurement::FULL_CLUSTER.id,
                                temperature_measurement::AttributeId::MeasuredValue as _,
                            )
                            .map(|(_, value)| value.unwrap().into_option())
                            .collect::<Vec<_>>(),
                        [Some(2350)]
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for shape rebuild subscription report");
                    }),
                )
                .await;
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
