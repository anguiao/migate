use super::*;

#[test]
fn move_color_preserves_each_axis_rate_until_its_own_boundary() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Light);
        service.publish(
            id.clone(),
            "RGB light",
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::Color,
            ]),
        );
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        let initial = RgbColor {
            red: 255,
            green: 0,
            blue: 0,
        };
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (Property::Color, PropertyValue::Color(initial)),
            ],
        );
        let commands = Rc::new(RecordingCommands::default());
        service.set_command_sink(commands.clone());
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let move_color = command_data(|writer| {
            writer.i16(&TLVTag::Context(0), 30_000).unwrap();
            writer.i16(&TLVTag::Context(1), 30_000).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let ctx = Context::command_at(
            &im,
            endpoint,
            color_control::FULL_CLUSTER.id,
            color_control::CommandId::MoveColor as _,
            &move_color,
        );
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        or(
            async {
                model.run(&ctx).await.unwrap();
            },
            async {
                async_io::Timer::after(Duration::from_millis(850)).await;
            },
        )
        .await;
        let actual = match &commands.0.borrow().last().unwrap().1[0] {
            DeviceCommand::SetColor(color) => *color,
            command => panic!("unexpected command: {command:?}"),
        };
        let (start_x, start_y) = rgb_to_xy(initial);
        let expected = (650_u64..=900).any(|elapsed_ms| {
            let advance = |start: u16| {
                (f64::from(start) + 30_000.0 * elapsed_ms as f64 / 1000.0)
                    .round()
                    .clamp(0.0, f64::from(0xfeff_u16)) as u16
            };
            let (red, green, blue) = SetDeviceColor::Xy {
                x: advance(start_x),
                y: advance(start_y),
            }
            .to_rgb(RgbGamma::SRgb);
            actual == RgbColor { red, green, blue }
        });
        assert!(expected, "each XY axis must advance at its requested rate");
    });
}

#[test]
fn lighting_adjustment_survives_state_reports_without_duplicate_dispatch() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Light);
        service.publish(
            id.clone(),
            "Light",
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::Brightness(NumericRange {
                    minimum: 0.0,
                    maximum: 100.0,
                    step: 1.0,
                    unit: NumericUnit::Percent,
                }),
            ]),
        );
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (
                    Property::Brightness,
                    PropertyValue::Percent(Percent::new(10.0).unwrap()),
                ),
            ],
        );
        let commands = Rc::new(ControlledCommands::new([
            CommandOutcome::Accepted,
            CommandOutcome::Accepted,
        ]));
        service.set_command_sink(commands.clone());
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let move_to = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 200).unwrap();
            writer.u16(&TLVTag::Context(1), 10).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let ctx = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &move_to,
        );
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert!(ctx.has_change(endpoint, 8, level_control::AttributeId::RemainingTime as _,));

        let exercise = async {
            let deadline = async_io::Timer::after(Duration::from_secs(2));
            futures_lite::pin!(deadline);
            while commands.calls.borrow().is_empty() {
                assert!(
                    poll_once(&mut deadline).await.is_none(),
                    "first step timed out"
                );
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            report(
                &service,
                &id,
                [(
                    Property::Brightness,
                    PropertyValue::Percent(Percent::new(11.0).unwrap()),
                )],
            );
            async_io::Timer::after(Duration::from_millis(10)).await;
            report(
                &service,
                &id,
                [(
                    Property::Brightness,
                    PropertyValue::Percent(Percent::new(12.0).unwrap()),
                )],
            );
            async_io::Timer::after(Duration::from_millis(150)).await;
            assert_eq!(
                commands.calls.borrow().len(),
                1,
                "held step was dispatched twice"
            );
            commands.release_next();
            while commands.calls.borrow().len() < 2 {
                assert!(
                    poll_once(&mut deadline).await.is_none(),
                    "next step timed out"
                );
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
        };
        or(
            async {
                let result = model.run(&ctx).await;
                panic!("bridge background stopped: {result:?}");
            },
            exercise,
        )
        .await;
    });
}

#[test]
fn rejected_adjustment_step_does_not_stop_lighting_background() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Light);
        service.publish(
            id.clone(),
            "Light",
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::Brightness(NumericRange {
                    minimum: 0.0,
                    maximum: 100.0,
                    step: 1.0,
                    unit: NumericUnit::Percent,
                }),
            ]),
        );
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (
                    Property::Brightness,
                    PropertyValue::Percent(Percent::new(10.0).unwrap()),
                ),
            ],
        );
        let commands = Rc::new(ControlledCommands::new([
            CommandOutcome::Rejected(-1),
            CommandOutcome::Accepted,
        ]));
        service.set_command_sink(commands.clone());
        let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let first = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 200).unwrap();
            writer.u16(&TLVTag::Context(1), 10).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let second = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 150).unwrap();
            writer.u16(&TLVTag::Context(1), 10).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let run_ctx = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &first,
        );
        model
            .invoke(
                &run_ctx,
                InvokeReplyInstance::new(run_ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        let next_ctx = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &second,
        );
        let exercise = async {
            let deadline = async_io::Timer::after(Duration::from_secs(2));
            futures_lite::pin!(deadline);
            while commands.calls.borrow().is_empty() {
                assert!(poll_once(&mut deadline).await.is_none());
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            commands.release_next();
            async_io::Timer::after(Duration::from_millis(30)).await;
            model
                .invoke(
                    &next_ctx,
                    InvokeReplyInstance::new(next_ctx.cmd(), WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap();
            while commands.calls.borrow().len() < 2 {
                assert!(poll_once(&mut deadline).await.is_none());
                async_io::Timer::after(Duration::from_millis(5)).await;
            }
            commands.release_next();
        };
        or(
            async {
                let result = model.run(&run_ctx).await;
                panic!("bridge background stopped: {result:?}");
            },
            exercise,
        )
        .await;
    });
}

#[test]
fn timed_off_extends_on_time_tracks_off_wait_and_honors_accept_only_when_on() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Load);
        service.publish(
            id.clone(),
            "Relay",
            FeatureCapabilities(vec![Capability::Power { writable: true }]),
        );
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        report(
            &service,
            &id,
            [(Property::Power, PropertyValue::Power(true))],
        );
        let commands = Rc::new(RecordingCommands::default());
        service.set_command_sink(commands.clone());
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let basic_info = crate::matter::common::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
        crate::matter::common::initialize_basic_info(&matter, &kv, true).unwrap();
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);
        let timed = |control, on_time, off_wait| {
            command_data(|writer| {
                writer.u8(&TLVTag::Context(0), control).unwrap();
                writer.u16(&TLVTag::Context(1), on_time).unwrap();
                writer.u16(&TLVTag::Context(2), off_wait).unwrap();
            })
        };
        let first_data = timed(0, 1, 2);
        let first = Context::command_at(
            &im,
            endpoint,
            6,
            on_off::CommandId::OnWithTimedOff as _,
            &first_data,
        );
        model
            .invoke(
                &first,
                InvokeReplyInstance::new(first.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        let exercise = async {
            async_io::Timer::after(Duration::from_millis(30)).await;
            let extend_data = timed(0, 3, 2);
            let extend = Context::command_at(
                &im,
                endpoint,
                6,
                on_off::CommandId::OnWithTimedOff as _,
                &extend_data,
            );
            model
                .invoke(
                    &extend,
                    InvokeReplyInstance::new(extend.cmd(), WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap();
            async_io::Timer::after(Duration::from_millis(130)).await;
            assert_eq!(commands.0.borrow().len(), 2, "timer was not extended");
            while commands.0.borrow().len() < 3 {
                async_io::Timer::after(Duration::from_millis(10)).await;
            }
            assert_eq!(commands.0.borrow()[2].1, [DeviceCommand::SetPower(false)]);
            let off_wait = Context::new_at(&im, endpoint, 6, on_off::AttributeId::OffWaitTime as _)
                .read_tlv(&model)
                .await;
            assert!(value_element(&off_wait).u16().unwrap() > 0);
            report(
                &service,
                &id,
                [(Property::Power, PropertyValue::Power(false))],
            );
            let accept_data = timed(on_off::OnOffControlBitmap::ACCEPT_ONLY_WHEN_ON.bits(), 5, 5);
            let accept = Context::command_at(
                &im,
                endpoint,
                6,
                on_off::CommandId::OnWithTimedOff as _,
                &accept_data,
            );
            model
                .invoke(
                    &accept,
                    InvokeReplyInstance::new(accept.cmd(), WriteBuf::new(&mut [0; 128])),
                )
                .await
                .unwrap();
            assert_eq!(commands.0.borrow().len(), 3);
            async_io::Timer::after(Duration::from_millis(230)).await;
            let off_wait = Context::new_at(&im, endpoint, 6, on_off::AttributeId::OffWaitTime as _)
                .read_tlv(&model)
                .await;
            assert_eq!(value_element(&off_wait).u16().unwrap(), 0);
        };
        or(
            async {
                let result = model.run(&first).await;
                panic!("bridge background stopped: {result:?}");
            },
            exercise,
        )
        .await;
    });
}

#[test]
fn real_im_level_subscription_throttles_rapid_reports_and_flushes_latest_value() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let id = feature(FeatureRole::Light);
                service.publish(
                    id.clone(),
                    "Light",
                    FeatureCapabilities(vec![
                        Capability::Power { writable: true },
                        Capability::Brightness(NumericRange {
                            minimum: 0.0,
                            maximum: 100.0,
                            step: 1.0,
                            unit: NumericUnit::Percent,
                        }),
                    ]),
                );
                let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
                report(
                    &service,
                    &id,
                    [
                        (Property::Power, PropertyValue::Power(true)),
                        (
                            Property::Brightness,
                            PropertyValue::Percent(Percent::new(10.0).unwrap()),
                        ),
                    ],
                );
                let model =
                    DeviceBridgeModel::new(service.clone(), store.devices(), store.matter())
                        .unwrap();
                let basic_info = crate::matter::common::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let kv = server.kv(crate::matter::storage::StoreAdapter::new(store.matter()));
                crate::matter::common::initialize_basic_info(&server, &kv, true).unwrap();
                connect(&server, 123456, 445566, 81);
                connect(&client, 445566, 123456, 81);
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
                let incoming = Pipe::default();
                let outgoing = Pipe::default();
                let responder = DefaultResponder::new(&im);
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
                    panic!("Matter service loop stopped before the level test completed");
                };
                let controller = async {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                    let subscription = subscribe_level(&client, endpoint).await.unwrap();
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    report(
                        &service,
                        &id,
                        [(
                            Property::Brightness,
                            PropertyValue::Percent(Percent::new(20.0).unwrap()),
                        )],
                    );
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    let report_data = ReportDataResp::from_tlv(&TLVElement::new(
                        exchange.rx().unwrap().payload(),
                    ))
                    .unwrap();
                    assert_eq!(report_data.subscription_id, Some(subscription));
                    assert_eq!(
                        report_data
                            .attrs::<Nullable<u8>>(
                                8,
                                level_control::AttributeId::CurrentLevel as _,
                            )
                            .map(|(_, value)| value.unwrap().into_option())
                            .collect::<Vec<_>>(),
                        [Some(51)]
                    );
                    exchange
                        .send_with(|_, buffer| {
                            StatusResp::write(buffer, IMStatusCode::Success)?;
                            Ok(Some(OpCode::StatusResponse.into()))
                        })
                        .await
                        .unwrap();
                    exchange.acknowledge().await.unwrap();
                    drop(exchange);
                    for value in [21.0, 22.0] {
                        report(
                            &service,
                            &id,
                            [(
                                Property::Brightness,
                                PropertyValue::Percent(Percent::new(value).unwrap()),
                            )],
                        );
                    }
                    let early = or(
                        async { Exchange::accept(&client).await.map(|_| true) },
                        async {
                            async_io::Timer::after(Duration::from_millis(150)).await;
                            Ok(false)
                        },
                    )
                    .await
                    .unwrap();
                    assert!(!early, "rapid level reports were not throttled");
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    let report_data = ReportDataResp::from_tlv(&TLVElement::new(
                        exchange.rx().unwrap().payload(),
                    ))
                    .unwrap();
                    assert_eq!(
                        report_data
                            .attrs::<Nullable<u8>>(
                                8,
                                level_control::AttributeId::CurrentLevel as _,
                            )
                            .map(|(_, value)| value.unwrap().into_option())
                            .collect::<Vec<_>>(),
                        [Some(56)]
                    );
                };
                or(services, controller).await;
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
