mod scenes;
mod transitions;

use super::*;

#[test]
fn color_temperature_light_declares_complete_lighting_shape() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Light);
    service.publish(
        id.clone(),
        "Ceiling light",
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
                step: 1.0,
                unit: NumericUnit::Kelvin,
            }),
        ]),
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();

    model.access(|node| {
        let light = node.endpoint(endpoint).expect("light endpoint");
        assert!(light.device_types.iter().any(|item| item.dtype == 0x010c));
        for cluster in [3, 4, 0x62, 6, 8, 0x0300, 57] {
            assert!(
                light.cluster(cluster).is_some(),
                "missing cluster {cluster:#x}"
            );
        }
    });
}

#[test]
fn lighting_shape_preserves_xy_without_inventing_level_or_temperature() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let mut rgb = feature(FeatureRole::Light);
    rgb.service_instance = 3;
    service.publish(
        rgb.clone(),
        "RGB light",
        FeatureCapabilities(vec![
            Capability::Power { writable: true },
            Capability::Color,
        ]),
    );
    let rgb_endpoint = store.devices().allocate_feature(&rgb).unwrap().endpoint;
    let mut ct = feature(FeatureRole::Light);
    ct.service_instance = 4;
    service.publish(
        ct.clone(),
        "CT light",
        FeatureCapabilities(vec![
            Capability::Power { writable: true },
            Capability::ColorTemperature(NumericRange {
                minimum: 2700.0,
                maximum: 6500.0,
                step: 100.0,
                unit: NumericUnit::Kelvin,
            }),
        ]),
    );
    let ct_endpoint = store.devices().allocate_feature(&ct).unwrap().endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();

    model.access(|node| {
        let rgb = node.endpoint(rgb_endpoint).unwrap();
        assert!(rgb.device_types.iter().any(|device| device.dtype == 0x0100));
        assert!(rgb.cluster(8).is_none());
        let color = rgb.cluster(0x0300).unwrap();
        assert_ne!(color.feature_map & color_control::Feature::XY.bits(), 0);
        assert_eq!(
            color.feature_map & color_control::Feature::COLOR_TEMPERATURE.bits(),
            0
        );

        let ct = node.endpoint(ct_endpoint).unwrap();
        assert!(ct.device_types.iter().any(|device| device.dtype == 0x0100));
        assert!(ct.cluster(8).is_none());
        let color = ct.cluster(0x0300).unwrap();
        assert_eq!(color.feature_map & color_control::Feature::XY.bits(), 0);
        assert_ne!(
            color.feature_map & color_control::Feature::COLOR_TEMPERATURE.bits(),
            0
        );
    });
}

#[test]
fn on_off_commands_use_the_queue_without_optimistic_state() {
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
            [(Property::Power, PropertyValue::Power(false))],
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
        let mut ctx = Context::new_at(&im, endpoint, 6, 0);
        ctx.set_command(1, TLVElement::new(&[0x15, 0x18]));
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow().as_slice(),
            &[(id.clone(), vec![DeviceCommand::SetPower(true)])]
        );
        let value = Context::new_at(&im, endpoint, 6, 0).read_tlv(&model).await;
        assert!(!value_element(&value).bool().unwrap());

        report(
            &service,
            &id,
            [(Property::Power, PropertyValue::Power(true))],
        );
        let value = Context::new_at(&im, endpoint, 6, 0).read_tlv(&model).await;
        assert!(value_element(&value).bool().unwrap());

        service.apply_unknown(&id, Property::Power, service.next_report_version());
        ctx.set_command(2, TLVElement::new(&[0x15, 0x18]));
        assert!(
            model
                .invoke(
                    &ctx,
                    InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
                )
                .await
                .is_err()
        );
        assert_eq!(commands.0.borrow().len(), 1);
    });
}

#[test]
fn level_and_color_temperature_use_confirmed_values_and_typed_commands() {
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
                    PropertyValue::Percent(Percent::new(0.001526).unwrap()),
                ),
                (
                    Property::ColorTemperature,
                    PropertyValue::ColorTemperature(4000),
                ),
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

        let level = Context::new_at(
            &im,
            endpoint,
            8,
            level_control::AttributeId::CurrentLevel as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(
            Nullable::<u8>::from_tlv(&value_element(&level)).unwrap(),
            Nullable::some(1)
        );
        let temperature = Context::new_at(
            &im,
            endpoint,
            0x0300,
            color_control::AttributeId::ColorTemperatureMireds as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(value_element(&temperature).u16().unwrap(), 250);

        let on_level = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 100).unwrap());
        let write = Context::write_at(
            &im,
            endpoint,
            8,
            level_control::AttributeId::OnLevel as _,
            &on_level,
        );
        model.write(&write).await.unwrap();
        let on = Context::command_at(&im, endpoint, 6, 1, &[0x15, 0x18]);
        model
            .invoke(
                &on,
                InvokeReplyInstance::new(on.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0].1,
            vec![
                DeviceCommand::SetBrightness(Percent::new(39.0).unwrap()),
                DeviceCommand::SetPower(true),
            ]
        );
        commands.0.borrow_mut().clear();
        assert!(
            Context::new_at(
                &im,
                endpoint,
                6,
                rs_matter::dm::clusters::decl::on_off::AttributeId::StartUpOnOff as _,
            )
            .read_tlv_result(&model)
            .await
            .is_err()
        );

        let step = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 0).unwrap();
            writer.u8(&TLVTag::Context(1), 10).unwrap();
            writer.null(&TLVTag::Context(2)).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
            writer.u8(&TLVTag::Context(4), 0).unwrap();
        });
        let ctx = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::StepWithOnOff as _,
            &step,
        );
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(commands.0.borrow().len(), 1);
        commands.0.borrow_mut().clear();
        let step_down = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 1).unwrap();
            writer.u8(&TLVTag::Context(1), 10).unwrap();
            writer.null(&TLVTag::Context(2)).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
            writer.u8(&TLVTag::Context(4), 0).unwrap();
        });
        let step_down = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::StepWithOnOff as _,
            &step_down,
        );
        model
            .invoke(
                &step_down,
                InvokeReplyInstance::new(step_down.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[0].1, [DeviceCommand::SetPower(false)]);
        commands.0.borrow_mut().clear();

        let ct = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 200).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let ctx = Context::command_at(
            &im,
            endpoint,
            0x0300,
            color_control::CommandId::MoveToColorTemperature as _,
            &ct,
        );
        model
            .invoke(
                &ctx,
                InvokeReplyInstance::new(ctx.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0],
            (id.clone(), vec![DeviceCommand::SetColorTemperature(5000)])
        );
        commands.0.borrow_mut().clear();

        let stepped_ct = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 270).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let stepped_ct = Context::command_at(
            &im,
            endpoint,
            0x0300,
            color_control::CommandId::MoveToColorTemperature as _,
            &stepped_ct,
        );
        model
            .invoke(
                &stepped_ct,
                InvokeReplyInstance::new(stepped_ct.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0].1,
            [DeviceCommand::SetColorTemperature(3700)]
        );
        commands.0.borrow_mut().clear();

        let invalid_level = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 255).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let invalid_level = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &invalid_level,
        );
        assert!(
            model
                .invoke(
                    &invalid_level,
                    InvokeReplyInstance::new(invalid_level.cmd(), WriteBuf::new(&mut [0; 128]),),
                )
                .await
                .is_err()
        );
        assert!(commands.0.borrow().is_empty());

        let long_transition = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 100).unwrap();
            writer.u16(&TLVTag::Context(1), 40_000).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let long_transition = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &long_transition,
        );
        model
            .invoke(
                &long_transition,
                InvokeReplyInstance::new(long_transition.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert!(commands.0.borrow().is_empty());

        let fade_off = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 0).unwrap();
            writer.u16(&TLVTag::Context(1), 1).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let fade_off = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevelWithOnOff as _,
            &fade_off,
        );
        model
            .invoke(
                &fade_off,
                InvokeReplyInstance::new(fade_off.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert!(commands.0.borrow().is_empty());
        or(
            async {
                model.run(&fade_off).await.unwrap();
            },
            async {
                async_io::Timer::after(Duration::from_millis(150)).await;
            },
        )
        .await;
        assert_eq!(
            commands.0.borrow().last().unwrap().1,
            [DeviceCommand::SetPower(false)]
        );

        let scene_handler = LightingHandler::new(
            service,
            id,
            vec![Capability::Power { writable: true }],
            endpoint,
            1,
        );
        scene_handler.begin_scene_recall(NonZeroU8::new(1).unwrap(), 1, 1);
        let mut bytes = vec![0; 64];
        let mut writer = WriteBuf::new(&mut bytes);
        writer.start_array(&TLVTag::Anonymous).unwrap();
        writer.start_struct(&TLVTag::Anonymous).unwrap();
        writer
            .u32(&TLVTag::Context(0), on_off::AttributeId::OnOff as _)
            .unwrap();
        writer.u8(&TLVTag::Context(1), 2).unwrap();
        writer.end_container().unwrap();
        writer.end_container().unwrap();
        let values =
            TLVArray::<AttributeValuePairStruct<'_>>::new(TLVElement::new(writer.as_slice()))
                .unwrap();
        assert!(
            SceneOnOff(&scene_handler)
                .apply(&fade_off, &values, 0)
                .await
                .is_err()
        );
    });
}

#[test]
fn matter_light_commands_reach_real_runtime_with_light3_wire_values() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "yeelink.light.light3",
            include_str!("../../../../tests/fixtures/miot_specs/yeelink.light.light3.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Light)
        .unwrap();
        let mut id = feature(FeatureRole::Light);
        id.service_instance = descriptor.definition.service_instance;
        service.publish(
            id.clone(),
            descriptor.definition.name.clone(),
            descriptor.definition.capabilities.clone(),
        );
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (
                    Property::Brightness,
                    PropertyValue::Percent(Percent::new(25.0).unwrap()),
                ),
                (
                    Property::ColorTemperature,
                    PropertyValue::ColorTemperature(4000),
                ),
            ],
        );
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

        let level = command_data(|writer| {
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
            &level,
        );
        let (result, ()) = zip(
            model.invoke(
                &level,
                InvokeReplyInstance::new(level.cmd(), WriteBuf::new(&mut [0; 128])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();
        assert_eq!(
            calls.borrow()[0],
            TransportCommand {
                device: id.physical.clone(),
                typed: DeviceCommand::SetBrightness(Percent::new(50.0).unwrap()),
                operation: WireOperation::SetProperty {
                    siid: 2,
                    piid: 2,
                    value: WireValue::Integer(32_768),
                },
            }
        );

        let color_temperature = command_data(|writer| {
            writer.u16(&TLVTag::Context(0), 200).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let color_temperature = Context::command_at(
            &im,
            endpoint,
            0x0300,
            color_control::CommandId::MoveToColorTemperature as _,
            &color_temperature,
        );
        let (result, ()) = zip(
            model.invoke(
                &color_temperature,
                InvokeReplyInstance::new(color_temperature.cmd(), WriteBuf::new(&mut [0; 128])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();
        assert_eq!(
            calls.borrow()[1],
            TransportCommand {
                device: id.physical,
                typed: DeviceCommand::SetColorTemperature(5000),
                operation: WireOperation::SetProperty {
                    siid: 2,
                    piid: 3,
                    value: WireValue::Integer(5000),
                },
            }
        );
    });
}

#[test]
fn reconcile_refreshes_lighting_ranges_without_rebuilding_the_endpoint() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Light);
        let capabilities = |step| {
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::Brightness(NumericRange {
                    minimum: 0.0,
                    maximum: 100.0,
                    step,
                    unit: NumericUnit::Percent,
                }),
            ])
        };
        service.publish(id.clone(), "Light", capabilities(1.0));
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
        let data = command_data(|writer| {
            writer.u8(&TLVTag::Context(0), 127).unwrap();
            writer.u16(&TLVTag::Context(1), 0).unwrap();
            writer.u8(&TLVTag::Context(2), 0).unwrap();
            writer.u8(&TLVTag::Context(3), 0).unwrap();
        });
        let command = Context::command_at(
            &im,
            endpoint,
            8,
            level_control::CommandId::MoveToLevel as _,
            &data,
        );
        let mut run = std::pin::pin!(model.run(&command));
        assert!(poll_once(&mut run).await.is_none());
        service.publish(id.clone(), "Light", capabilities(30.0));
        assert!(poll_once(&mut run).await.is_none());
        model
            .invoke(
                &command,
                InvokeReplyInstance::new(command.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0].1,
            [DeviceCommand::SetBrightness(Percent::new(60.0).unwrap())]
        );
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());
    });
}
