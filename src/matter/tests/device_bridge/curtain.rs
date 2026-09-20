use super::*;

#[test]
fn real_curtain_declares_a_window_covering_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let descriptor = compile_spec(
        "xiaomi.curtain.acn010",
        include_str!("../../../../tests/fixtures/miot_specs/xiaomi.curtain.acn010.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.definition.role == FeatureRole::Curtain)
    .unwrap();
    let mut id = feature(FeatureRole::Curtain);
    id.service_instance = descriptor.definition.service_instance;
    service.publish(
        id.clone(),
        descriptor.definition.name,
        descriptor.definition.capabilities,
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        let curtain = node.endpoint(endpoint).expect("window covering endpoint");
        assert!(curtain.device_types.iter().any(|item| item.dtype == 0x0202));
        for cluster in [3, 4, window_covering::FULL_CLUSTER.id, 57] {
            assert!(
                curtain.cluster(cluster).is_some(),
                "missing cluster {cluster:#x}"
            );
        }
        let window = curtain.cluster(window_covering::FULL_CLUSTER.id).unwrap();
        assert_eq!(
            window.feature_map,
            (window_covering::Feature::LIFT | window_covering::Feature::POSITION_AWARE_LIFT).bits()
        );
        for attribute in [
            window_covering::AttributeId::CurrentPositionLiftPercent100ths,
            window_covering::AttributeId::TargetPositionLiftPercent100ths,
        ] {
            assert!(window.attribute(attribute as _).is_some());
        }
        for attribute in [
            window_covering::AttributeId::CurrentPositionTiltPercent100ths,
            window_covering::AttributeId::TargetPositionTiltPercent100ths,
            window_covering::AttributeId::CurrentPositionLift,
        ] {
            assert!(window.attribute(attribute as _).is_none());
        }
        for command in [
            window_covering::CommandId::UpOrOpen,
            window_covering::CommandId::DownOrClose,
            window_covering::CommandId::StopMotion,
            window_covering::CommandId::GoToLiftPercentage,
        ] {
            assert!(window.command(command as _).is_some());
        }
        for command in [
            window_covering::CommandId::GoToLiftValue,
            window_covering::CommandId::GoToTiltValue,
            window_covering::CommandId::GoToTiltPercentage,
        ] {
            assert!(window.command(command as _).is_none());
        }
    });
}

#[test]
fn curtain_without_a_real_stop_action_does_not_create_a_window_covering_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Curtain);
    service.publish(
        id.clone(),
        "Curtain",
        FeatureCapabilities(vec![Capability::CurtainPosition(NumericRange {
            minimum: 0.0,
            maximum: 100.0,
            step: 1.0,
            unit: NumericUnit::Percent,
        })]),
    );
    store.devices().allocate_feature(&id).unwrap();

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    assert_eq!(model.endpoint_for(&id), None);
}

#[test]
fn curtain_tlv_inverts_confirmed_positions_and_dispatches_only_valid_commands() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "xiaomi.curtain.acn010",
            include_str!("../../../../tests/fixtures/miot_specs/xiaomi.curtain.acn010.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Curtain)
        .unwrap();
        let mut id = feature(FeatureRole::Curtain);
        id.service_instance = descriptor.definition.service_instance;
        service.publish(
            id.clone(),
            descriptor.definition.name,
            descriptor.definition.capabilities,
        );
        service.admit(&id);
        report(
            &service,
            &id,
            [
                (
                    Property::CurtainPosition,
                    PropertyValue::Percent(Percent::new(25.0).unwrap()),
                ),
                (
                    Property::CurtainTargetPosition,
                    PropertyValue::Percent(Percent::new(80.0).unwrap()),
                ),
                (
                    Property::CurtainMovement,
                    PropertyValue::CurtainMovement(crate::device::CurtainMovement::Opening),
                ),
            ],
        );
        let commands = Rc::new(CurtainCommands::default());
        service.set_command_sink(commands.clone());
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
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
        let read =
            |attribute| Context::new_at(&im, endpoint, window_covering::FULL_CLUSTER.id, attribute);

        assert_eq!(
            window_covering::Type::from_tlv(&value_element(
                &read(window_covering::AttributeId::Type as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap(),
            window_covering::Type::Unknown
        );
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::CurrentPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option(),
            Some(7500)
        );
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::TargetPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option(),
            Some(2000)
        );
        assert_eq!(
            window_covering::OperationalStatus::from_tlv(&value_element(
                &read(window_covering::AttributeId::OperationalStatus as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .bits(),
            0x05
        );
        assert_eq!(
            window_covering::ConfigStatus::from_tlv(&value_element(
                &read(window_covering::AttributeId::ConfigStatus as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap(),
            window_covering::ConfigStatus::OPERATIONAL
                | window_covering::ConfigStatus::LIFT_POSITION_AWARE
        );

        for (command, expected) in [
            (
                window_covering::CommandId::UpOrOpen,
                DeviceCommand::SetCurtainPosition(Percent::new(100.0).unwrap()),
            ),
            (
                window_covering::CommandId::DownOrClose,
                DeviceCommand::SetCurtainPosition(Percent::new(0.0).unwrap()),
            ),
        ] {
            let data = command_data(|_| {});
            let ctx = Context::command_at(
                &im,
                endpoint,
                window_covering::FULL_CLUSTER.id,
                command as _,
                &data,
            );
            model
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 64])),
                )
                .await
                .unwrap();
            assert_eq!(commands.calls.borrow().last().unwrap().1, [expected]);
        }

        let position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 2500).unwrap();
        });
        let ctx = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &position,
        );
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.calls.borrow().last().unwrap().1,
            [DeviceCommand::SetCurtainPosition(
                Percent::new(75.0).unwrap()
            )]
        );

        let invalid_position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 2550).unwrap();
        });
        let invalid = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &invalid_position,
        );
        assert!(
            model
                .invoke(
                    &invalid,
                    InvokeReplyInstance::new(invalid.cmd(), WriteBuf::new(&mut [0; 64])),
                )
                .await
                .is_err()
        );
        assert_eq!(commands.calls.borrow().len(), 3);

        let stop_data = command_data(|_| {});
        let stop = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::StopMotion as _,
            &stop_data,
        );
        model
            .invoke(
                &stop,
                InvokeReplyInstance::new(stop.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.stopped.borrow().as_slice(),
            [(id.clone(), Property::CurtainTargetPosition)]
        );
        assert_eq!(
            commands.calls.borrow().last().unwrap().1,
            [DeviceCommand::StopCurtain]
        );

        for raw in [0_u8, 1, 2, 4, 8, 16] {
            let mode = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, raw).unwrap());
            let result = model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    window_covering::FULL_CLUSTER.id,
                    window_covering::AttributeId::Mode as _,
                    &mode,
                ))
                .await;
            assert_eq!(result.is_ok(), raw == 0);
        }
        let too_wide = scalar_data(|writer| writer.u16(&TLVTag::Anonymous, 256).unwrap());
        assert!(
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    window_covering::FULL_CLUSTER.id,
                    window_covering::AttributeId::Mode as _,
                    &too_wide,
                ))
                .await
                .is_err()
        );
        assert_eq!(commands.calls.borrow().len(), 4);
        assert!(
            window_covering::Mode::from_tlv(&value_element(
                &read(window_covering::AttributeId::Mode as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .is_empty()
        );

        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::CurrentPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option(),
            Some(7500),
            "accepted commands must not invent a position"
        );
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::TargetPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option(),
            Some(2000),
            "accepted commands must not invent a target"
        );
        assert_eq!(
            window_covering::OperationalStatus::from_tlv(&value_element(
                &read(window_covering::AttributeId::OperationalStatus as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .bits(),
            0x05,
            "accepted commands must not invent motion"
        );
        service.apply_unknown(
            &id,
            Property::CurtainPosition,
            service.next_report_version(),
        );
        service.apply_unknown(
            &id,
            Property::CurtainTargetPosition,
            service.next_report_version(),
        );
        service.apply_unknown(
            &id,
            Property::CurtainMovement,
            service.next_report_version(),
        );
        assert!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::CurrentPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option()
            .is_none()
        );
        assert!(
            Nullable::<u16>::from_tlv(&value_element(
                &read(window_covering::AttributeId::TargetPositionLiftPercent100ths as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap()
            .into_option()
            .is_none()
        );
        assert!(
            read(window_covering::AttributeId::OperationalStatus as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
    });
}

#[test]
fn curtain_commands_reach_runtime_and_stop_retracts_an_unsent_target() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "xiaomi.curtain.acn010",
            include_str!("../../../../tests/fixtures/miot_specs/xiaomi.curtain.acn010.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Curtain)
        .unwrap();
        let mut id = feature(FeatureRole::Curtain);
        id.physical.parent_did = DeviceDid::new("xiaomi.curtain.acn010").unwrap();
        id.service_instance = descriptor.definition.service_instance;
        service.publish(
            id.clone(),
            descriptor.definition.name.clone(),
            descriptor.definition.capabilities.clone(),
        );
        service.admit(&id);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let runtime =
            CommandRuntime::new(service.clone(), Rc::new(CapturingTransport(calls.clone())));
        runtime.register(RuntimeFeature {
            identity: id.clone(),
            descriptor,
            authority_generation: 1,
            auth_session_generation: store.xiaomi().snapshot().unwrap().session_generation,
        });
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
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

        let position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 2500).unwrap();
        });
        let target = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &position,
        );
        let (result, ()) = zip(
            model.invoke(
                &target,
                InvokeReplyInstance::new(target.cmd(), WriteBuf::new(&mut [0; 64])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();
        assert_eq!(
            calls.borrow().as_slice(),
            [TransportCommand {
                device: id.physical.clone(),
                typed: DeviceCommand::SetCurtainPosition(Percent::new(75.0).unwrap()),
                operation: WireOperation::SetProperty {
                    siid: 2,
                    piid: 4,
                    value: WireValue::Integer(75),
                },
            }]
        );

        let queued_position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 6000).unwrap();
        });
        let queued_target = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &queued_position,
        );
        let mut queued_reply_bytes = [0; 64];
        let queued_reply =
            InvokeReplyInstance::new(queued_target.cmd(), WriteBuf::new(&mut queued_reply_bytes));
        let mut queued = std::pin::pin!(model.invoke(&queued_target, queued_reply));
        assert!(poll_once(&mut queued).await.is_none());

        let stop_data = command_data(|_| {});
        let stop = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::StopMotion as _,
            &stop_data,
        );
        let (result, ()) = zip(
            model.invoke(
                &stop,
                InvokeReplyInstance::new(stop.cmd(), WriteBuf::new(&mut [0; 64])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();
        assert!(poll_once(&mut queued).await.unwrap().is_err());
        assert_eq!(
            calls.borrow().len(),
            2,
            "the withdrawn target reached transport"
        );
        assert_eq!(
            calls.borrow()[1],
            TransportCommand {
                device: id.physical,
                typed: DeviceCommand::StopCurtain,
                operation: WireOperation::SetProperty {
                    siid: 2,
                    piid: 1,
                    value: WireValue::Integer(2),
                },
            }
        );
    });
}

#[test]
fn reconcile_refreshes_curtain_grid_and_reports_each_confirmed_state() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Curtain);
        let capabilities = |step| {
            FeatureCapabilities(vec![
                Capability::CurtainPosition(NumericRange {
                    minimum: 0.0,
                    maximum: 100.0,
                    step,
                    unit: NumericUnit::Percent,
                }),
                Capability::CurtainStop,
            ])
        };
        service.publish(id.clone(), "Curtain", capabilities(5.0));
        service.admit(&id);
        let commands = Rc::new(RecordingCommands::default());
        service.set_command_sink(commands.clone());
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
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
        let ctx = Context::new_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::AttributeId::CurrentPositionLiftPercent100ths as _,
        );
        let mut run = std::pin::pin!(model.run(&ctx));
        assert!(poll_once(&mut run).await.is_none());

        let position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 8500).unwrap();
        });
        let target = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &position,
        );
        model
            .invoke(
                &target,
                InvokeReplyInstance::new(target.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        assert_eq!(commands.0.borrow().len(), 1);

        service.publish(id.clone(), "Curtain", capabilities(10.0));
        assert!(poll_once(&mut run).await.is_none());
        assert!(
            model
                .invoke(
                    &target,
                    InvokeReplyInstance::new(target.cmd(), WriteBuf::new(&mut [0; 64])),
                )
                .await
                .is_err(),
            "the reconciled 10% grid accepted a 15% target"
        );
        assert_eq!(commands.0.borrow().len(), 1);
        let valid_position = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 8000).unwrap();
        });
        let valid = Context::command_at(
            &im,
            endpoint,
            window_covering::FULL_CLUSTER.id,
            window_covering::CommandId::GoToLiftPercentage as _,
            &valid_position,
        );
        model
            .invoke(
                &valid,
                InvokeReplyInstance::new(valid.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        assert_eq!(commands.0.borrow().len(), 2);
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());

        for attribute in [
            window_covering::AttributeId::CurrentPositionLiftPercent100ths,
            window_covering::AttributeId::TargetPositionLiftPercent100ths,
            window_covering::AttributeId::OperationalStatus,
        ] {
            assert!(!ctx.has_change(endpoint, window_covering::FULL_CLUSTER.id, attribute as _));
        }

        report(
            &service,
            &id,
            [
                (
                    Property::CurtainPosition,
                    PropertyValue::Percent(Percent::new(20.0).unwrap()),
                ),
                (
                    Property::CurtainTargetPosition,
                    PropertyValue::Percent(Percent::new(30.0).unwrap()),
                ),
                (
                    Property::CurtainMovement,
                    PropertyValue::CurtainMovement(crate::device::CurtainMovement::Closing),
                ),
            ],
        );
        assert!(poll_once(&mut run).await.is_none());
        for attribute in [
            window_covering::AttributeId::CurrentPositionLiftPercent100ths,
            window_covering::AttributeId::TargetPositionLiftPercent100ths,
            window_covering::AttributeId::OperationalStatus,
        ] {
            assert!(ctx.has_change(endpoint, window_covering::FULL_CLUSTER.id, attribute as _));
        }
    });
}

#[test]
fn real_im_curtain_groups_and_position_subscription_use_the_dynamic_endpoint() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let descriptor = compile_spec(
                    "xiaomi.curtain.acn010",
                    include_str!(
                        "../../../../tests/fixtures/miot_specs/xiaomi.curtain.acn010.json"
                    ),
                )
                .unwrap()
                .features
                .into_iter()
                .find(|feature| feature.definition.role == FeatureRole::Curtain)
                .unwrap();
                let mut id = feature(FeatureRole::Curtain);
                id.service_instance = descriptor.definition.service_instance;
                service.publish(
                    id.clone(),
                    descriptor.definition.name,
                    descriptor.definition.capabilities,
                );
                report(
                    &service,
                    &id,
                    [
                        (
                            Property::CurtainPosition,
                            PropertyValue::Percent(Percent::new(20.0).unwrap()),
                        ),
                        (
                            Property::CurtainTargetPosition,
                            PropertyValue::Percent(Percent::new(20.0).unwrap()),
                        ),
                        (
                            Property::CurtainMovement,
                            PropertyValue::CurtainMovement(crate::device::CurtainMovement::Stopped),
                        ),
                    ],
                );
                let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
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
                connect(&server, 123456, 445566, 82);
                connect(&client, 445566, 123456, 82);
                server.with_state(|state| {
                    state
                        .fabrics
                        .fabric_mut(NonZeroU8::new(1).unwrap())
                        .unwrap()
                        .groups_mut()
                        .key_map_add(GroupKeyMapping {
                            group_id: 0x1234,
                            group_key_set_id: 1,
                        })
                        .unwrap();
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
                    panic!("Matter service loop stopped before the curtain controller completed");
                };
                let controller = async {
                    let add_group = command_data(|writer| {
                        writer.u16(&TLVTag::Context(0), 0x1234).unwrap();
                        writer.utf8(&TLVTag::Context(1), "Curtains").unwrap();
                    });
                    invoke_command(
                        &client,
                        NonZeroU8::new(1).unwrap(),
                        endpoint,
                        groups::FULL_CLUSTER.id,
                        groups::CommandId::AddGroup as _,
                        &add_group,
                    )
                    .await
                    .unwrap();
                    assert_eq!(
                        group_membership(&client, NonZeroU8::new(1).unwrap(), endpoint)
                            .await
                            .unwrap(),
                        [0x1234]
                    );

                    let subscription = subscribe_curtain_position(&client, endpoint).await.unwrap();
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
                            Property::CurtainPosition,
                            PropertyValue::Percent(Percent::new(40.0).unwrap()),
                        )],
                    );
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    {
                        let rx = exchange.rx().unwrap();
                        let report =
                            ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                        assert_eq!(report.subscription_id, Some(subscription));
                        assert_eq!(
                            report
                                .attrs::<Nullable<u16>>(
                                    window_covering::FULL_CLUSTER.id,
                                    window_covering::AttributeId::CurrentPositionLiftPercent100ths
                                        as _,
                                )
                                .map(|(_, value)| value.unwrap().into_option())
                                .collect::<Vec<_>>(),
                            [Some(6000)]
                        );
                    }
                    exchange
                        .send_with(|_, buffer| {
                            StatusResp::write(buffer, IMStatusCode::Success)?;
                            Ok(Some(OpCode::StatusResponse.into()))
                        })
                        .await
                        .unwrap();
                    exchange.acknowledge().await.unwrap();
                    drop(exchange);

                    report(
                        &service,
                        &id,
                        [(
                            Property::CurtainPosition,
                            PropertyValue::Percent(Percent::new(40.0).unwrap()),
                        )],
                    );
                    let duplicate = or(
                        async { Exchange::accept(&client).await.map(|_| true) },
                        async {
                            async_io::Timer::after(Duration::from_millis(100)).await;
                            Ok(false)
                        },
                    )
                    .await
                    .unwrap();
                    assert!(!duplicate, "same curtain position produced a second report");
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for curtain Groups/subscription behavior");
                    }),
                )
                .await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
