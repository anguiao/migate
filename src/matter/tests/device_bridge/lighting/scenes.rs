use super::*;

#[test]
fn level_options_couple_temperature_and_global_scene_recalls_confirmed_values() {
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
                Capability::ColorTemperature(NumericRange {
                    minimum: 2700.0,
                    maximum: 6500.0,
                    step: 100.0,
                    unit: NumericUnit::Kelvin,
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
                    PropertyValue::Percent(Percent::new(40.0).unwrap()),
                ),
                (
                    Property::ColorTemperature,
                    PropertyValue::ColorTemperature(4000),
                ),
            ],
        );
        let commands = Rc::new(RecordingCommands::default());
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
        let options = scalar_data(|writer| {
            writer
                .u8(
                    &TLVTag::Anonymous,
                    (level_control::OptionsBitmap::EXECUTE_IF_OFF
                        | level_control::OptionsBitmap::COUPLE_COLOR_TEMP_TO_LEVEL)
                        .bits(),
                )
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                8,
                level_control::AttributeId::Options as _,
                &options,
            ))
            .await
            .unwrap();
        let move_to = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 127).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let level = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &move_to,
        );
        model
            .invoke(
                &level,
                InvokeReplyInstance::new(level.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0].1,
            [
                DeviceCommand::SetBrightness(Percent::new(50.0).unwrap()),
                DeviceCommand::SetColorTemperature(3800),
            ]
        );
        let effect_data = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 0).unwrap();
            writer.u8(&TLVTag::Context(1), 0).unwrap();
        });
        let effect = Context::command_at(
            &im,
            endpoint,
            6,
            on_off::CommandId::OffWithEffect as _,
            &effect_data,
        );
        model
            .invoke(
                &effect,
                InvokeReplyInstance::new(effect.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        let global = Context::new_at(
            &im,
            endpoint,
            6,
            on_off::AttributeId::GlobalSceneControl as _,
        )
        .read_tlv(&model)
        .await;
        assert!(!value_element(&global).bool().unwrap());
        let recall = Context::command_at(
            &im,
            endpoint,
            6,
            on_off::CommandId::OnWithRecallGlobalScene as _,
            &[0x15, 0x18],
        );
        model
            .invoke(
                &recall,
                InvokeReplyInstance::new(recall.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[2].1,
            [
                DeviceCommand::SetPower(true),
                DeviceCommand::SetBrightness(Percent::new(40.0).unwrap()),
                DeviceCommand::SetColorTemperature(4000),
            ]
        );
        let global = Context::new_at(
            &im,
            endpoint,
            6,
            on_off::AttributeId::GlobalSceneControl as _,
        )
        .read_tlv(&model)
        .await;
        assert!(value_element(&global).bool().unwrap());
    });
}

#[test]
fn real_im_scenes_are_endpoint_and_fabric_scoped_and_require_confirmed_state() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let first = feature(FeatureRole::Light);
                let mut second = first.clone();
                second.service_instance = 3;
                let capabilities = FeatureCapabilities(vec![
                    Capability::Power { writable: true },
                    Capability::Brightness(NumericRange {
                        minimum: 0.0,
                        maximum: 100.0,
                        step: 1.0,
                        unit: NumericUnit::Percent,
                    }),
                ]);
                service.publish(first.clone(), "First", capabilities.clone());
                service.publish(second.clone(), "Second", capabilities);
                let first_endpoint = store.devices().allocate_feature(&first).unwrap().endpoint;
                let second_endpoint = store.devices().allocate_feature(&second).unwrap().endpoint;
                let scene_level = Percent::new(30.000_762_951_094_835).unwrap();
                for id in [&first, &second] {
                    report(
                        &service,
                        id,
                        [
                            (Property::Power, PropertyValue::Power(true)),
                            (Property::Brightness, PropertyValue::Percent(scene_level)),
                        ],
                    );
                }
                let commands = Rc::new(ControlledCommands::new([
                    CommandOutcome::Accepted,
                    CommandOutcome::Accepted,
                ]));
                service.set_command_sink(commands.clone());
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
                connect(&server, 123456, 445566, 71);
                connect(&client, 445566, 123456, 71);
                let fabric_two = NonZeroU8::new(2).unwrap();
                connect_at(&server, fabric_two, 123456, 445577, 72);
                connect_at(&client, fabric_two, 445577, 123456, 72);
                server.with_state(|state| {
                    for fabric in [NonZeroU8::new(1).unwrap(), fabric_two] {
                        state
                            .fabrics
                            .fabric_mut(fabric)
                            .unwrap()
                            .groups_mut()
                            .key_map_add(GroupKeyMapping {
                                group_id: 0x0329,
                                group_key_set_id: 1,
                            })
                            .unwrap();
                    }
                });
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
                    panic!("Matter service loop stopped before the scene test completed");
                };
                let controller = async {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                    let fabric_one = NonZeroU8::new(1).unwrap();
                    let group_id = 0x0329;
                    let add_group = command_data(|writer| {
                        writer.u16(&TLVTag::Context(0), group_id).unwrap();
                        writer.utf8(&TLVTag::Context(1), "Room").unwrap();
                    });
                    let scene = |scene_id| {
                        command_data(|writer| {
                            writer.u16(&TLVTag::Context(0), group_id).unwrap();
                            writer.u8(&TLVTag::Context(1), scene_id).unwrap();
                        })
                    };
                    for (fabric, endpoint) in [
                        (fabric_one, first_endpoint),
                        (fabric_one, second_endpoint),
                        (fabric_two, first_endpoint),
                    ] {
                        invoke_command(
                            &client,
                            fabric,
                            endpoint,
                            4,
                            groups::CommandId::AddGroup as _,
                            &add_group,
                        )
                        .await
                        .unwrap();
                    }
                    invoke_command(
                        &client,
                        fabric_one,
                        first_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::StoreScene as _,
                        &scene(1),
                    )
                    .await
                    .unwrap();
                    report(
                        &service,
                        &first,
                        [(
                            Property::Brightness,
                            PropertyValue::Percent(Percent::new(60.0).unwrap()),
                        )],
                    );
                    futures_lite::future::yield_now().await;
                    invoke_command(
                        &client,
                        fabric_one,
                        first_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::StoreScene as _,
                        &scene(2),
                    )
                    .await
                    .unwrap();
                    invoke_command(
                        &client,
                        fabric_one,
                        second_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::StoreScene as _,
                        &scene(1),
                    )
                    .await
                    .unwrap();
                    invoke_command(
                        &client,
                        fabric_two,
                        first_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::StoreScene as _,
                        &scene(1),
                    )
                    .await
                    .unwrap();
                    for scene_id in 3..=16 {
                        invoke_command(
                            &client,
                            fabric_one,
                            first_endpoint,
                            scenes_management::FULL_CLUSTER.id,
                            scenes_management::CommandId::StoreScene as _,
                            &scene(scene_id),
                        )
                        .await
                        .unwrap();
                    }
                    assert_eq!(
                        scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap(),
                        (16, 16, group_id, true, 0)
                    );
                    assert_eq!(
                        scene_info(&client, fabric_one, second_endpoint)
                            .await
                            .unwrap(),
                        (1, 1, group_id, true, 15)
                    );
                    assert_eq!(
                        scene_info(&client, fabric_two, first_endpoint)
                            .await
                            .unwrap(),
                        (1, 1, group_id, true, 15)
                    );
                    report(
                        &service,
                        &first,
                        [
                            (Property::Power, PropertyValue::Power(false)),
                            (
                                Property::Brightness,
                                PropertyValue::Percent(Percent::new(60.0).unwrap()),
                            ),
                        ],
                    );
                    futures_lite::future::yield_now().await;
                    let scene_one = scene(1);
                    let recall = invoke_command(
                        &client,
                        fabric_one,
                        first_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::RecallScene as _,
                        &scene_one,
                    );
                    futures_lite::pin!(recall);
                    while commands.calls.borrow().is_empty() {
                        assert!(poll_once(&mut recall).await.is_none());
                        futures_lite::future::yield_now().await;
                    }
                    commands.release_next();
                    while commands.calls.borrow().len() < 2 {
                        assert!(poll_once(&mut recall).await.is_none());
                        futures_lite::future::yield_now().await;
                    }
                    report(
                        &service,
                        &first,
                        [(Property::Power, PropertyValue::Power(true))],
                    );
                    futures_lite::future::yield_now().await;
                    assert!(
                        !scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap()
                            .3,
                        "a partial scene report must not confirm recall before every cluster applies"
                    );
                    commands.release_next();
                    recall.await.unwrap();
                    assert!(
                        !scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap()
                            .3
                    );
                    assert_eq!(
                        scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap()
                            .2,
                        group_id
                    );
                    report(
                        &service,
                        &first,
                        [
                            (Property::Power, PropertyValue::Power(true)),
                            (Property::Brightness, PropertyValue::Percent(scene_level)),
                        ],
                    );
                    futures_lite::future::yield_now().await;
                    assert!(
                        scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap()
                            .3
                    );
                    report(
                        &service,
                        &first,
                        [(
                            Property::Brightness,
                            PropertyValue::Percent(Percent::new(40.0).unwrap()),
                        )],
                    );
                    futures_lite::future::yield_now().await;
                    assert!(
                        !scene_info(&client, fabric_one, first_endpoint)
                            .await
                            .unwrap()
                            .3
                    );
                };
                or(services, controller).await;

                let restored_commands = Rc::new(RecordingCommands::default());
                service.set_command_sink(restored_commands.clone());
                let restored_model =
                    DeviceBridgeModel::new(service.clone(), store.devices(), store.matter())
                        .unwrap();
                let restored_server =
                    Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let restored_client =
                    Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let restored_crypto = test_only_crypto();
                let restored_buffers: MatterBuffers = MatterBuffers::new();
                let restored_state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let restored_kv = restored_server
                    .kv(crate::matter::storage::StoreAdapter::new(store.matter()));
                crate::matter::common::initialize_basic_info(&restored_server, &restored_kv, true)
                    .unwrap();
                connect(&restored_server, 123456, 445566, 73);
                connect(&restored_client, 445566, 123456, 73);
                let fabric_two = NonZeroU8::new(2).unwrap();
                connect_at(&restored_server, fabric_two, 123456, 445577, 74);
                connect_at(&restored_client, fabric_two, 445577, 123456, 74);
                restored_server.with_state(|state| {
                    for fabric in [NonZeroU8::new(1).unwrap(), fabric_two] {
                        state
                            .fabrics
                            .fabric_mut(fabric)
                            .unwrap()
                            .groups_mut()
                            .key_map_add(GroupKeyMapping {
                                group_id: 0x0329,
                                group_key_set_id: 1,
                            })
                            .unwrap();
                    }
                });
                let mut restored_random = rand::rng();
                let restored_handler = endpoints::EthSysHandlerBuilder::new()
                    .netif_diag(&SysNetifs)
                    .build(&mut restored_random)
                    .chain(|endpoint, _| endpoint != 0, &restored_model);
                let restored_im = InteractionModel::new(
                    &restored_server,
                    &restored_crypto,
                    &restored_buffers,
                    (&restored_model, &restored_handler),
                    &restored_kv,
                    &restored_state,
                );
                let restored_incoming = Pipe::default();
                let restored_outgoing = Pipe::default();
                let restored_responder = DefaultResponder::new(&restored_im);
                restored_im.startup().await.unwrap();
                let restored_services = async {
                    or(
                        restored_server.run(
                            &restored_crypto,
                            SendPipe(&restored_outgoing),
                            ReceivePipe(&restored_incoming),
                            NoNetwork,
                        ),
                        or(
                            restored_client.run(
                                &restored_crypto,
                                SendPipe(&restored_incoming),
                                ReceivePipe(&restored_outgoing),
                                NoNetwork,
                            ),
                            or(restored_responder.run::<4, 4>(), restored_im.run()),
                        ),
                    )
                    .await
                    .unwrap();
                    panic!("restored Matter scene service stopped unexpectedly");
                };
                let restored_controller = async {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                    let fabric_one = NonZeroU8::new(1).unwrap();
                    assert_eq!(
                        scene_info(&restored_client, fabric_one, first_endpoint)
                            .await
                            .unwrap(),
                        (16, 1, 0x0329, false, 0)
                    );
                    assert_eq!(
                        scene_info(&restored_client, fabric_two, first_endpoint)
                            .await
                            .unwrap()
                            .0,
                        1
                    );
                    let add_group = command_data(|writer| {
                        writer.u16(&TLVTag::Context(0), 0x0329).unwrap();
                        writer.utf8(&TLVTag::Context(1), "Room").unwrap();
                    });
                    invoke_command(
                        &restored_client,
                        fabric_two,
                        first_endpoint,
                        groups::FULL_CLUSTER.id,
                        groups::CommandId::AddGroup as _,
                        &add_group,
                    )
                    .await
                    .unwrap();
                    let scene = command_data(|writer| {
                        writer.u16(&TLVTag::Context(0), 0x0329).unwrap();
                        writer.u8(&TLVTag::Context(1), 1).unwrap();
                    });
                    invoke_command(
                        &restored_client,
                        fabric_two,
                        first_endpoint,
                        scenes_management::FULL_CLUSTER.id,
                        scenes_management::CommandId::RecallScene as _,
                        &scene,
                    )
                    .await
                    .unwrap();
                    assert!(restored_commands.0.borrow().iter().any(|(_, commands)| {
                        commands.iter().any(|command| {
                            matches!(command, DeviceCommand::SetBrightness(_))
                        })
                    }));

                    restored_model
                        .lifecycle(
                            &restored_im,
                            rs_matter::dm::LifecycleOp::FabricRemoval {
                                fab_idx: fabric_one,
                            },
                        )
                        .unwrap();
                    assert!(
                        scene_info(&restored_client, fabric_one, first_endpoint)
                            .await
                            .is_err()
                    );
                    assert!(
                        scene_info(&restored_client, fabric_one, second_endpoint)
                            .await
                            .is_err()
                    );
                    assert_eq!(
                        scene_info(&restored_client, fabric_two, first_endpoint)
                            .await
                            .unwrap()
                            .0,
                        1
                    );
                };
                or(restored_services, restored_controller).await;
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
