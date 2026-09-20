use super::*;

#[test]
fn discrete_fan_declares_only_the_real_fan_shape() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let descriptor = compile_spec(
        "dmaker.fan.p5c",
        include_str!("../../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.definition.role == FeatureRole::Fan)
    .unwrap();
    let mut id = feature(FeatureRole::Fan);
    id.service_instance = descriptor.definition.service_instance;
    service.publish(
        id.clone(),
        descriptor.definition.name,
        descriptor.definition.capabilities,
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();

    model.access(|node| {
        let fan = node.endpoint(endpoint).expect("fan endpoint");
        assert!(fan.device_types.iter().any(|item| item.dtype == 0x002b));
        for cluster in [3, 4, fan_control::FULL_CLUSTER.id, 57] {
            assert!(
                fan.cluster(cluster).is_some(),
                "missing cluster {cluster:#x}"
            );
        }
        for cluster in [6, 8, 0x0300, 0x0062] {
            assert!(
                fan.cluster(cluster).is_none(),
                "unexpected cluster {cluster:#x}"
            );
        }
    });
}

#[test]
fn fan_tlv_reads_and_writes_use_confirmed_discrete_state_without_optimism() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "dmaker.fan.p5c",
            include_str!("../../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Fan)
        .unwrap();
        let mut id = feature(FeatureRole::Fan);
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
                (Property::Power, PropertyValue::Power(true)),
                (Property::FanSpeed, PropertyValue::FanSpeed(2)),
                (Property::Oscillation, PropertyValue::Oscillation(false)),
            ],
        );
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
        let read =
            |attribute| Context::new_at(&im, endpoint, fan_control::FULL_CLUSTER.id, attribute);

        assert_eq!(
            value_element(
                &read(fan_control::AttributeId::FanMode as _)
                    .read_tlv(&model)
                    .await
            )
            .u8()
            .unwrap(),
            fan_control::FanModeEnum::Medium as u8
        );
        assert_eq!(
            value_element(
                &read(fan_control::AttributeId::FanModeSequence as _)
                    .read_tlv(&model)
                    .await
            )
            .u8()
            .unwrap(),
            fan_control::FanModeSequenceEnum::OffLowMedHigh as u8
        );
        for (attribute, expected) in [
            (fan_control::AttributeId::PercentSetting, 50),
            (fan_control::AttributeId::PercentCurrent, 50),
            (fan_control::AttributeId::SpeedMax, 4),
            (fan_control::AttributeId::SpeedSetting, 2),
            (fan_control::AttributeId::SpeedCurrent, 2),
            (fan_control::AttributeId::RockSupport, 1),
            (fan_control::AttributeId::RockSetting, 0),
        ] {
            assert_eq!(
                value_element(&read(attribute as _).read_tlv(&model).await)
                    .u8()
                    .unwrap(),
                expected,
                "{attribute:?}"
            );
        }

        let percent = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 35).unwrap());
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentSetting as _,
                &percent,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[0].1, [DeviceCommand::SetFanSpeed(2)]);
        assert_eq!(
            value_element(
                &read(fan_control::AttributeId::PercentCurrent as _)
                    .read_tlv(&model)
                    .await
            )
            .u8()
            .unwrap(),
            50
        );

        let null = scalar_data(|writer| writer.null(&TLVTag::Anonymous).unwrap());
        for attribute in [
            fan_control::AttributeId::PercentSetting,
            fan_control::AttributeId::SpeedSetting,
        ] {
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    fan_control::FULL_CLUSTER.id,
                    attribute as _,
                    &null,
                ))
                .await
                .unwrap();
        }
        assert_eq!(commands.0.borrow().len(), 1);

        let on = scalar_data(|writer| {
            writer
                .u8(&TLVTag::Anonymous, fan_control::FanModeEnum::On as u8)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::FanMode as _,
                &on,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[1].1, [DeviceCommand::SetFanSpeed(4)]);
        assert_eq!(
            value_element(
                &read(fan_control::AttributeId::PercentCurrent as _)
                    .read_tlv(&model)
                    .await
            )
            .u8()
            .unwrap(),
            50
        );
        report(
            &service,
            &id,
            [(Property::FanSpeed, PropertyValue::FanSpeed(4))],
        );
        assert_eq!(
            value_element(
                &read(fan_control::AttributeId::PercentCurrent as _)
                    .read_tlv(&model)
                    .await
            )
            .u8()
            .unwrap(),
            100
        );

        let smart = scalar_data(|writer| {
            writer
                .u8(&TLVTag::Anonymous, fan_control::FanModeEnum::Smart as u8)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::FanMode as _,
                &smart,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[2].1, [DeviceCommand::SetFanSpeed(4)]);
        for (attribute, value) in [
            (
                fan_control::AttributeId::FanMode,
                fan_control::FanModeEnum::Auto as u8,
            ),
            (fan_control::AttributeId::FanMode, 255),
            (fan_control::AttributeId::PercentSetting, 101),
            (fan_control::AttributeId::SpeedSetting, 5),
        ] {
            let data = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, value).unwrap());
            assert!(
                model
                    .write(&Context::write_at(
                        &im,
                        endpoint,
                        fan_control::FULL_CLUSTER.id,
                        attribute as _,
                        &data,
                    ))
                    .await
                    .is_err()
            );
        }
        assert_eq!(commands.0.borrow().len(), 3);

        let unsupported_rock = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 2).unwrap());
        assert!(
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    fan_control::FULL_CLUSTER.id,
                    fan_control::AttributeId::RockSetting as _,
                    &unsupported_rock,
                ))
                .await
                .is_err()
        );
        assert_eq!(commands.0.borrow().len(), 3);

        service.apply_unknown(&id, Property::FanSpeed, service.next_report_version());
        assert!(
            read(fan_control::AttributeId::PercentCurrent as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
        assert!(
            read(fan_control::AttributeId::SpeedCurrent as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );

        let off = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 0).unwrap());
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentSetting as _,
                &off,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[3].1, [DeviceCommand::SetPower(false)]);
        assert!(
            read(fan_control::AttributeId::PercentCurrent as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
        report(
            &service,
            &id,
            [(Property::Power, PropertyValue::Power(false))],
        );
        for attribute in [
            fan_control::AttributeId::FanMode,
            fan_control::AttributeId::PercentSetting,
            fan_control::AttributeId::PercentCurrent,
            fan_control::AttributeId::SpeedSetting,
            fan_control::AttributeId::SpeedCurrent,
        ] {
            assert_eq!(
                value_element(&read(attribute as _).read_tlv(&model).await)
                    .u8()
                    .unwrap(),
                0,
                "{attribute:?}"
            );
        }
    });
}

#[test]
fn bath_heater_supply_and_exhaust_are_independent_power_only_fans() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let compiled = compile_spec(
            "yeelink.bhf_light.v13",
            include_str!("../../../../tests/fixtures/miot_specs/yeelink.bhf_light.v13.json"),
        )
        .unwrap();
        let mut ids = Vec::new();
        for role in [
            FeatureRole::BathHeaterSupplyFan,
            FeatureRole::BathHeaterExhaustFan,
        ] {
            let descriptor = compiled
                .features
                .iter()
                .find(|feature| feature.definition.role == role)
                .unwrap();
            let mut id = feature(role);
            id.service_instance = descriptor.definition.service_instance;
            service.publish(
                id.clone(),
                descriptor.definition.name.clone(),
                descriptor.definition.capabilities.clone(),
            );
            service.admit(&id);
            store.devices().allocate_feature(&id).unwrap();
            ids.push(id);
        }
        report(
            &service,
            &ids[0],
            [(Property::Power, PropertyValue::Power(true))],
        );
        report(
            &service,
            &ids[1],
            [(Property::Power, PropertyValue::Power(false))],
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

        for (index, expected_mode, expected_percent) in [
            (0, fan_control::FanModeEnum::High, 100),
            (1, fan_control::FanModeEnum::Off, 0),
        ] {
            let endpoint = model.endpoint_for(&ids[index]).unwrap();
            model.access(|node| {
                let fan = node.endpoint(endpoint).unwrap();
                let cluster = fan.cluster(fan_control::FULL_CLUSTER.id).unwrap();
                assert!(
                    cluster
                        .attribute(fan_control::AttributeId::SpeedMax as _)
                        .is_none()
                );
            });
            let mode = Context::new_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::FanMode as _,
            )
            .read_tlv(&model)
            .await;
            assert_eq!(value_element(&mode).u8().unwrap(), expected_mode as u8);
            let percent = Context::new_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentCurrent as _,
            )
            .read_tlv(&model)
            .await;
            assert_eq!(value_element(&percent).u8().unwrap(), expected_percent);
        }

        let on = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 100).unwrap());
        let exhaust = model.endpoint_for(&ids[1]).unwrap();
        model
            .write(&Context::write_at(
                &im,
                exhaust,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentSetting as _,
                &on,
            ))
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow().as_slice(),
            [(ids[1].clone(), vec![DeviceCommand::SetPower(true)])]
        );
    });
}

#[test]
fn matter_fan_commands_reach_real_runtime_with_p5c_wire_values() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "dmaker.fan.p5c",
            include_str!("../../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Fan)
        .unwrap();
        let mut id = feature(FeatureRole::Fan);
        id.service_instance = descriptor.definition.service_instance;
        service.publish(
            id.clone(),
            descriptor.definition.name.clone(),
            descriptor.definition.capabilities.clone(),
        );
        service.admit(&id);
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(false)),
                (Property::FanSpeed, PropertyValue::FanSpeed(1)),
                (Property::Oscillation, PropertyValue::Oscillation(false)),
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

        let percent = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 35).unwrap());
        let write = Context::write_at(
            &im,
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::PercentSetting as _,
            &percent,
        );
        let (result, ()) = zip(model.write(&write), runtime.run_until_idle()).await;
        result.unwrap();
        assert_eq!(
            calls.borrow().as_slice(),
            [
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::SetFanSpeed(2),
                    operation: WireOperation::SetProperty {
                        siid: 2,
                        piid: 2,
                        value: WireValue::Integer(2),
                    },
                },
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::SetPower(true),
                    operation: WireOperation::SetProperty {
                        siid: 2,
                        piid: 1,
                        value: WireValue::Boolean(true),
                    },
                },
            ]
        );

        let rock = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 1).unwrap());
        let write = Context::write_at(
            &im,
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::RockSetting as _,
            &rock,
        );
        let (result, ()) = zip(model.write(&write), runtime.run_until_idle()).await;
        result.unwrap();
        assert_eq!(
            calls.borrow()[2],
            TransportCommand {
                device: id.physical,
                typed: DeviceCommand::SetOscillation(true),
                operation: WireOperation::SetProperty {
                    siid: 2,
                    piid: 4,
                    value: WireValue::Boolean(true),
                },
            }
        );
    });
}

#[test]
fn auto_fan_keeps_auto_distinct_from_off_and_unknown_percent() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Fan);
        service.publish(
            id.clone(),
            "Auto fan",
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::FanSpeeds(vec![0, 1, 2]),
            ]),
        );
        service.admit(&id);
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (Property::FanSpeed, PropertyValue::FanSpeed(0)),
            ],
        );
        let commands = Rc::new(RecordingCommands::default());
        service.set_command_sink(commands.clone());
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

        let mode = Context::new_at(
            &im,
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::FanMode as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(
            value_element(&mode).u8().unwrap(),
            fan_control::FanModeEnum::Auto as u8
        );
        let setting = Context::new_at(
            &im,
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::PercentSetting as _,
        )
        .read_tlv(&model)
        .await;
        assert!(
            Nullable::<u8>::from_tlv(&value_element(&setting))
                .unwrap()
                .into_option()
                .is_none()
        );
        assert!(
            Context::new_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentCurrent as _,
            )
            .read_tlv_result(&model)
            .await
            .is_err()
        );

        let smart = scalar_data(|writer| {
            writer
                .u8(&TLVTag::Anonymous, fan_control::FanModeEnum::Smart as u8)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::FanMode as _,
                &smart,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow()[0].1, [DeviceCommand::SetFanSpeed(0)]);
    });
}

#[test]
fn reconcile_refreshes_fan_levels_without_rebuilding_the_endpoint() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Fan);
        let capabilities = |levels| {
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::FanSpeeds(levels),
            ])
        };
        service.publish(id.clone(), "Fan", capabilities(vec![1, 2, 3, 4]));
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (Property::FanSpeed, PropertyValue::FanSpeed(2)),
            ],
        );
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
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::SpeedMax as _,
        );
        let mut run = std::pin::pin!(model.run(&ctx));
        assert!(poll_once(&mut run).await.is_none());

        service.publish(id.clone(), "Fan", capabilities(vec![10, 20]));
        report(
            &service,
            &id,
            [(Property::FanSpeed, PropertyValue::FanSpeed(20))],
        );
        assert!(poll_once(&mut run).await.is_none());

        let speed_max = ctx.read_tlv(&model).await;
        assert_eq!(value_element(&speed_max).u8().unwrap(), 2);
        let percent = Context::new_at(
            &im,
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::PercentCurrent as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(value_element(&percent).u8().unwrap(), 100);
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());
        assert!(ctx.has_change(
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::SpeedMax as _,
        ));
    });
}

#[test]
fn real_im_fan_subscription_reports_confirmed_changes_once() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let id = feature(FeatureRole::Fan);
                service.publish(
                    id.clone(),
                    "Fan",
                    FeatureCapabilities(vec![
                        Capability::Power { writable: true },
                        Capability::FanSpeeds(vec![1, 2, 3, 4]),
                    ]),
                );
                report(
                    &service,
                    &id,
                    [
                        (Property::Power, PropertyValue::Power(true)),
                        (Property::FanSpeed, PropertyValue::FanSpeed(1)),
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
                connect(&server, 123456, 445566, 72);
                connect(&client, 445566, 123456, 72);
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
                    panic!("Matter service loop stopped before the fan controller completed");
                };
                let controller = async {
                    let subscription = subscribe_fan_percent(&client, endpoint).await.unwrap();
                    while !state
                        .subscriptions()
                        .has_subscription_for(NonZeroU8::new(1).unwrap(), 445566)
                    {
                        futures_lite::future::yield_now().await;
                    }
                    report(
                        &service,
                        &id,
                        [(Property::FanSpeed, PropertyValue::FanSpeed(2))],
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
                                .attrs::<u8>(
                                    fan_control::FULL_CLUSTER.id,
                                    fan_control::AttributeId::PercentCurrent as _,
                                )
                                .map(|(_, value)| value.unwrap())
                                .collect::<Vec<_>>(),
                            [50]
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
                        [(Property::FanSpeed, PropertyValue::FanSpeed(2))],
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
                    assert!(!duplicate, "same fan value produced a second Matter report");
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for the fan subscription");
                    }),
                )
                .await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
