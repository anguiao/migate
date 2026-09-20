use super::*;

#[test]
fn reconcile_refreshes_thermostat_range_without_rebuilding_the_endpoint() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Climate);
        let capabilities = |minimum, maximum| {
            FeatureCapabilities(vec![
                Capability::Power { writable: true },
                Capability::TargetTemperature(NumericRange {
                    minimum,
                    maximum,
                    step: 1.0,
                    unit: NumericUnit::Celsius,
                }),
                Capability::HvacModes(vec![HvacMode::Cool, HvacMode::Heat]),
            ])
        };
        service.publish(id.clone(), "Climate", capabilities(16.0, 30.0));
        let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Cool)),
                (
                    Property::TargetTemperature,
                    PropertyValue::Temperature(24.0),
                ),
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
            thermostat::FULL_CLUSTER.id,
            thermostat::AttributeId::AbsMinCoolSetpointLimit as _,
        );
        let mut run = std::pin::pin!(model.run(&ctx));
        assert!(poll_once(&mut run).await.is_none());

        service.publish(id.clone(), "Climate", capabilities(18.0, 28.0));
        assert!(poll_once(&mut run).await.is_none());

        assert_eq!(
            value_element(&ctx.read_tlv(&model).await).i16().unwrap(),
            1800
        );
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());
        assert!(ctx.has_change(
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::AttributeId::AbsMinCoolSetpointLimit as _,
        ));
    });
}

#[test]
fn ancillary_temperature_on_climate_role_does_not_create_a_sensor_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Climate);
    service.publish(
        id.clone(),
        "Air conditioner",
        FeatureCapabilities(vec![Capability::Temperature(NumericRange {
            minimum: 16.0,
            maximum: 32.0,
            step: 1.0,
            unit: NumericUnit::Celsius,
        })]),
    );
    store.devices().allocate_feature(&id).unwrap();

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    assert_eq!(model.endpoint_for(&id), None);
}

#[test]
fn real_air_conditioner_declares_thermostat_and_fan_control_on_one_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let descriptor = compile_spec(
        "lumi.acpartner.mcn04",
        include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.definition.role == FeatureRole::Climate)
    .unwrap();
    let mut id = feature(FeatureRole::Climate);
    id.service_instance = descriptor.definition.service_instance;
    service.publish(
        id.clone(),
        descriptor.definition.name,
        descriptor.definition.capabilities,
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        let climate = node.endpoint(endpoint).expect("thermostat endpoint");
        assert!(climate.device_types.iter().any(|item| item.dtype == 0x0301));
        for cluster in [
            3,
            4,
            thermostat::FULL_CLUSTER.id,
            fan_control::FULL_CLUSTER.id,
            57,
        ] {
            assert!(
                climate.cluster(cluster).is_some(),
                "missing cluster {cluster:#x}"
            );
        }
        for cluster in [6, 8, 0x0300, 0x0062] {
            assert!(
                climate.cluster(cluster).is_none(),
                "unexpected cluster {cluster:#x}"
            );
        }
        let thermostat = climate.cluster(thermostat::FULL_CLUSTER.id).unwrap();
        assert_eq!(
            thermostat.feature_map,
            (thermostat::Feature::HEATING
                | thermostat::Feature::COOLING
                | thermostat::Feature::LOCAL_TEMPERATURE_NOT_EXPOSED)
                .bits()
        );
        for attribute in [
            thermostat::AttributeId::OccupiedHeatingSetpoint,
            thermostat::AttributeId::OccupiedCoolingSetpoint,
            thermostat::AttributeId::AbsMinHeatSetpointLimit,
            thermostat::AttributeId::AbsMaxHeatSetpointLimit,
            thermostat::AttributeId::AbsMinCoolSetpointLimit,
            thermostat::AttributeId::AbsMaxCoolSetpointLimit,
        ] {
            assert!(thermostat.attribute(attribute as _).is_some());
        }
        for attribute in [
            thermostat::AttributeId::MinHeatSetpointLimit,
            thermostat::AttributeId::MaxHeatSetpointLimit,
            thermostat::AttributeId::MinCoolSetpointLimit,
            thermostat::AttributeId::MaxCoolSetpointLimit,
            thermostat::AttributeId::MinSetpointDeadBand,
        ] {
            assert!(thermostat.attribute(attribute as _).is_none());
        }
    });
}

#[test]
fn climate_without_a_heating_or_cooling_mode_does_not_create_a_thermostat_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Climate);
    service.publish(
        id.clone(),
        "Climate",
        FeatureCapabilities(vec![
            Capability::Power { writable: true },
            Capability::TargetTemperature(NumericRange {
                minimum: 16.0,
                maximum: 30.0,
                step: 1.0,
                unit: NumericUnit::Celsius,
            }),
        ]),
    );
    store.devices().allocate_feature(&id).unwrap();

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    assert_eq!(model.endpoint_for(&id), None);
}

#[test]
fn thermostat_tlv_uses_confirmed_mode_single_target_and_climate_fan_controls() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "lumi.acpartner.mcn04",
            include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Climate)
        .unwrap();
        let mut id = feature(FeatureRole::Climate);
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
                (Property::Power, PropertyValue::Power(false)),
                (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Cool)),
                (
                    Property::TargetTemperature,
                    PropertyValue::Temperature(24.0),
                ),
                (Property::FanSpeed, PropertyValue::FanSpeed(2)),
                (
                    Property::SwingMode,
                    PropertyValue::SwingMode(crate::device::SwingMode::Off),
                ),
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
            |attribute| Context::new_at(&im, endpoint, thermostat::FULL_CLUSTER.id, attribute);

        let local = read(thermostat::AttributeId::LocalTemperature as _)
            .read_tlv(&model)
            .await;
        assert!(
            Nullable::<i16>::from_tlv(&value_element(&local))
                .unwrap()
                .into_option()
                .is_none()
        );
        let mode = read(thermostat::AttributeId::SystemMode as _)
            .read_tlv(&model)
            .await;
        assert_eq!(
            thermostat::SystemModeEnum::from_tlv(&value_element(&mode)).unwrap(),
            thermostat::SystemModeEnum::Off
        );
        let cool = read(thermostat::AttributeId::OccupiedCoolingSetpoint as _)
            .read_tlv(&model)
            .await;
        assert_eq!(value_element(&cool).i16().unwrap(), 2400);
        assert!(
            read(thermostat::AttributeId::OccupiedHeatingSetpoint as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );

        let setpoint = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 2500).unwrap());
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                &setpoint,
            ))
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[0].1,
            [DeviceCommand::SetTargetTemperature(25.0)]
        );
        assert_eq!(
            value_element(
                &read(thermostat::AttributeId::OccupiedCoolingSetpoint as _)
                    .read_tlv(&model)
                    .await
            )
            .i16()
            .unwrap(),
            2400
        );
        let invalid = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 2450).unwrap());
        assert!(
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    thermostat::FULL_CLUSTER.id,
                    thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                    &invalid,
                ))
                .await
                .is_err()
        );
        let out_of_range = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 3100).unwrap());
        assert!(
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    thermostat::FULL_CLUSTER.id,
                    thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                    &out_of_range,
                ))
                .await
                .is_err()
        );

        report(
            &service,
            &id,
            [(
                Property::TargetTemperature,
                PropertyValue::Temperature(16.0),
            )],
        );
        let raise = command_data(|writer| {
            thermostat::SetpointRaiseLowerModeEnum::Cool
                .to_tlv(&TLVTag::Context(0), &mut *writer)
                .unwrap();
            writer.i8(&TLVTag::Context(1), -10).unwrap();
        });
        let raise = Context::command_at(
            &im,
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::CommandId::SetpointRaiseLower as _,
            &raise,
        );
        model
            .invoke(
                &raise,
                InvokeReplyInstance::new(raise.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[1].1,
            [DeviceCommand::SetTargetTemperature(16.0)]
        );

        let sequence = read(thermostat::AttributeId::ControlSequenceOfOperation as _)
            .read_tlv(&model)
            .await;
        assert_eq!(
            thermostat::ControlSequenceOfOperationEnum::from_tlv(&value_element(&sequence))
                .unwrap(),
            thermostat::ControlSequenceOfOperationEnum::CoolingAndHeating
        );
        let heating_only = scalar_data(|writer| {
            thermostat::ControlSequenceOfOperationEnum::HeatingOnly
                .to_tlv(&TLVTag::Anonymous, writer)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::ControlSequenceOfOperation as _,
                &heating_only,
            ))
            .await
            .unwrap();
        assert_eq!(commands.0.borrow().len(), 2);
        let sequence = read(thermostat::AttributeId::ControlSequenceOfOperation as _)
            .read_tlv(&model)
            .await;
        assert_eq!(
            thermostat::ControlSequenceOfOperationEnum::from_tlv(&value_element(&sequence))
                .unwrap(),
            thermostat::ControlSequenceOfOperationEnum::CoolingAndHeating
        );

        let heat = scalar_data(|writer| {
            thermostat::SystemModeEnum::Heat
                .to_tlv(&TLVTag::Anonymous, writer)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::SystemMode as _,
                &heat,
            ))
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[2].1,
            [
                DeviceCommand::SetHvacMode(HvacMode::Heat),
                DeviceCommand::SetPower(true),
            ]
        );
        assert_eq!(
            thermostat::SystemModeEnum::from_tlv(&value_element(
                &read(thermostat::AttributeId::SystemMode as _)
                    .read_tlv(&model)
                    .await
            ))
            .unwrap(),
            thermostat::SystemModeEnum::Off
        );

        let auto = scalar_data(|writer| {
            thermostat::SystemModeEnum::Auto
                .to_tlv(&TLVTag::Anonymous, writer)
                .unwrap()
        });
        assert!(
            model
                .write(&Context::write_at(
                    &im,
                    endpoint,
                    thermostat::FULL_CLUSTER.id,
                    thermostat::AttributeId::SystemMode as _,
                    &auto,
                ))
                .await
                .is_err()
        );

        let percent = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 100).unwrap());
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
        assert_eq!(
            commands.0.borrow()[3].1,
            [DeviceCommand::SetFanSpeed(3), DeviceCommand::SetPower(true)]
        );
        let rock = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 2).unwrap());
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::RockSetting as _,
                &rock,
            ))
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[4].1,
            [DeviceCommand::SetSwingMode(
                crate::device::SwingMode::Vertical
            )]
        );

        let system_mode = read(thermostat::AttributeId::SystemMode as _);
        let mut run = std::pin::pin!(model.run(&system_mode));
        assert!(poll_once(&mut run).await.is_none());
        for (cluster, attribute) in [
            (
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::SystemMode as _,
            ),
            (
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::FanMode as _,
            ),
        ] {
            assert!(!system_mode.has_change(endpoint, cluster, attribute));
        }
        report(
            &service,
            &id,
            [(Property::Power, PropertyValue::Power(true))],
        );
        assert!(poll_once(&mut run).await.is_none());
        assert!(system_mode.has_change(
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::AttributeId::SystemMode as _,
        ));
        assert!(system_mode.has_change(
            endpoint,
            fan_control::FULL_CLUSTER.id,
            fan_control::AttributeId::FanMode as _,
        ));

        report(
            &service,
            &id,
            [
                (Property::Power, PropertyValue::Power(true)),
                (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Auto)),
            ],
        );
        assert!(
            read(thermostat::AttributeId::SystemMode as _)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
    });
}

#[test]
fn bath_heater_thermostat_controls_only_heating_and_its_target() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "yeelink.bhf_light.v13",
            include_str!("../../../../tests/fixtures/miot_specs/yeelink.bhf_light.v13.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::BathHeaterClimate)
        .unwrap();
        let mut id = feature(FeatureRole::BathHeaterClimate);
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
                (Property::Power, PropertyValue::Power(false)),
                (
                    Property::TargetTemperature,
                    PropertyValue::Temperature(30.0),
                ),
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

        model.access(|node| {
            let cluster = node
                .endpoint(endpoint)
                .unwrap()
                .cluster(thermostat::FULL_CLUSTER.id)
                .unwrap();
            assert!(
                cluster
                    .attribute(thermostat::AttributeId::OccupiedHeatingSetpoint as _)
                    .is_some()
            );
            assert!(
                cluster
                    .attribute(thermostat::AttributeId::OccupiedCoolingSetpoint as _)
                    .is_none()
            );
            assert!(
                node.endpoint(endpoint)
                    .unwrap()
                    .cluster(fan_control::FULL_CLUSTER.id)
                    .is_none()
            );
        });
        let heating = Context::new_at(
            &im,
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::AttributeId::OccupiedHeatingSetpoint as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(value_element(&heating).i16().unwrap(), 3000);

        let target = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 3100).unwrap());
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedHeatingSetpoint as _,
                &target,
            ))
            .await
            .unwrap();
        let heat = scalar_data(|writer| {
            thermostat::SystemModeEnum::Heat
                .to_tlv(&TLVTag::Anonymous, writer)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::SystemMode as _,
                &heat,
            ))
            .await
            .unwrap();

        let both = command_data(|writer| {
            thermostat::SetpointRaiseLowerModeEnum::Both
                .to_tlv(&TLVTag::Context(0), &mut *writer)
                .unwrap();
            writer.i8(&TLVTag::Context(1), 10).unwrap();
        });
        let both = Context::command_at(
            &im,
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::CommandId::SetpointRaiseLower as _,
            &both,
        );
        model
            .invoke(
                &both,
                InvokeReplyInstance::new(both.cmd(), WriteBuf::new(&mut [0; 64])),
            )
            .await
            .unwrap();
        let cool = command_data(|writer| {
            thermostat::SetpointRaiseLowerModeEnum::Cool
                .to_tlv(&TLVTag::Context(0), &mut *writer)
                .unwrap();
            writer.i8(&TLVTag::Context(1), 10).unwrap();
        });
        let cool = Context::command_at(
            &im,
            endpoint,
            thermostat::FULL_CLUSTER.id,
            thermostat::CommandId::SetpointRaiseLower as _,
            &cool,
        );
        assert_eq!(
            model
                .invoke(
                    &cool,
                    InvokeReplyInstance::new(cool.cmd(), WriteBuf::new(&mut [0; 64])),
                )
                .await
                .unwrap_err()
                .code(),
            rs_matter::error::ErrorCode::InvalidCommand
        );
        let off = scalar_data(|writer| {
            thermostat::SystemModeEnum::Off
                .to_tlv(&TLVTag::Anonymous, writer)
                .unwrap()
        });
        model
            .write(&Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::SystemMode as _,
                &off,
            ))
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow().as_slice(),
            [
                (id.clone(), vec![DeviceCommand::SetTargetTemperature(31.0)]),
                (id.clone(), vec![DeviceCommand::SetPower(true)]),
                (id.clone(), vec![DeviceCommand::SetTargetTemperature(31.0)]),
                (id, vec![DeviceCommand::SetPower(false)]),
            ]
        );
    });
}

#[test]
fn thermostat_commands_reach_runtime_with_each_companions_real_wire_mapping() {
    block_on(async {
        for (
            model_name,
            document,
            service_instance,
            cool_raw,
            target_piid,
            fan_service,
            fan_level_piid,
            swing_piid,
        ) in [
            (
                "lumi.acpartner.mcn02",
                include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"),
                2,
                1,
                3,
                3,
                1,
                2,
            ),
            (
                "lumi.acpartner.mcn04",
                include_str!("../../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
                3,
                0,
                4,
                4,
                2,
                4,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path()).unwrap();
            let identity = store.load_identity().unwrap();
            let service = DeviceService::new();
            let descriptor = compile_spec(model_name, document)
                .unwrap()
                .features
                .into_iter()
                .find(|feature| feature.definition.role == FeatureRole::Climate)
                .unwrap();
            let mut id = feature(FeatureRole::Climate);
            id.physical.parent_did = DeviceDid::new(model_name).unwrap();
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
                    (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Heat)),
                    (
                        Property::TargetTemperature,
                        PropertyValue::Temperature(23.0),
                    ),
                    (Property::FanSpeed, PropertyValue::FanSpeed(2)),
                    (
                        Property::SwingMode,
                        PropertyValue::SwingMode(crate::device::SwingMode::Off),
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
            let im =
                InteractionModel::new(&matter, &crypto, &buffers, (&model, &model), &kv, &state);

            let cool = scalar_data(|writer| {
                thermostat::SystemModeEnum::Cool
                    .to_tlv(&TLVTag::Anonymous, writer)
                    .unwrap()
            });
            let write = Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::SystemMode as _,
                &cool,
            );
            let (result, ()) = zip(model.write(&write), runtime.run_until_idle()).await;
            result.unwrap();

            let target = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 2400).unwrap());
            let unconfirmed_cooling_target = Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                &target,
            );
            assert!(model.write(&unconfirmed_cooling_target).await.is_err());
            report(
                &service,
                &id,
                [
                    (Property::Power, PropertyValue::Power(true)),
                    (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Cool)),
                ],
            );

            let target = scalar_data(|writer| writer.i16(&TLVTag::Anonymous, 2400).unwrap());
            let write = Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedHeatingSetpoint as _,
                &target,
            );
            assert!(model.write(&write).await.is_err());
            let target = Context::write_at(
                &im,
                endpoint,
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                &target,
            );
            let (result, ()) = zip(model.write(&target), runtime.run_until_idle()).await;
            result.unwrap();

            let percent = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 100).unwrap());
            let write = Context::write_at(
                &im,
                endpoint,
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentSetting as _,
                &percent,
            );
            let (result, ()) = zip(model.write(&write), runtime.run_until_idle()).await;
            result.unwrap();

            let rock = scalar_data(|writer| writer.u8(&TLVTag::Anonymous, 2).unwrap());
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
                calls.borrow().as_slice(),
                [
                    TransportCommand {
                        device: id.physical.clone(),
                        typed: DeviceCommand::SetHvacMode(HvacMode::Cool),
                        operation: WireOperation::SetProperty {
                            siid: service_instance,
                            piid: 2,
                            value: WireValue::Integer(cool_raw),
                        },
                    },
                    TransportCommand {
                        device: id.physical.clone(),
                        typed: DeviceCommand::SetPower(true),
                        operation: WireOperation::SetProperty {
                            siid: service_instance,
                            piid: 1,
                            value: WireValue::Boolean(true),
                        },
                    },
                    TransportCommand {
                        device: id.physical.clone(),
                        typed: DeviceCommand::SetTargetTemperature(24.0),
                        operation: WireOperation::SetProperty {
                            siid: service_instance,
                            piid: target_piid,
                            value: WireValue::Integer(24),
                        },
                    },
                    TransportCommand {
                        device: id.physical.clone(),
                        typed: DeviceCommand::SetFanSpeed(3),
                        operation: WireOperation::SetProperty {
                            siid: fan_service,
                            piid: fan_level_piid,
                            value: WireValue::Integer(3),
                        },
                    },
                    TransportCommand {
                        device: id.physical.clone(),
                        typed: DeviceCommand::SetSwingMode(crate::device::SwingMode::Vertical),
                        operation: WireOperation::SetProperty {
                            siid: fan_service,
                            piid: swing_piid,
                            value: WireValue::Boolean(true),
                        },
                    },
                ]
            );
            if model_name == "lumi.acpartner.mcn02" {
                let legacy = Mcn02LegacyMapping::new();
                assert_eq!(
                    legacy.encode(&calls.borrow()[0].typed).unwrap(),
                    LegacyMiioOperation {
                        method: "set_mode",
                        arguments: vec![WireValue::String("cool".into())],
                    }
                );
                assert_eq!(
                    legacy.encode(&calls.borrow()[2].typed).unwrap(),
                    LegacyMiioOperation {
                        method: "set_tar_temp",
                        arguments: vec![WireValue::Integer(24)],
                    }
                );
                assert_eq!(
                    legacy.encode(&calls.borrow()[3].typed).unwrap(),
                    LegacyMiioOperation {
                        method: "set_fan_level",
                        arguments: vec![WireValue::String("large_fan".into())],
                    }
                );
                assert_eq!(
                    legacy.encode(&calls.borrow()[4].typed).unwrap(),
                    LegacyMiioOperation {
                        method: "set_ver_swing",
                        arguments: vec![WireValue::String("on".into())],
                    }
                );
            }
        }
    });
}

#[test]
fn real_im_thermostat_subscription_reports_confirmed_changes_once() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let id = feature(FeatureRole::Climate);
                service.publish(
                    id.clone(),
                    "Climate",
                    FeatureCapabilities(vec![
                        Capability::Power { writable: true },
                        Capability::TargetTemperature(NumericRange {
                            minimum: 16.0,
                            maximum: 30.0,
                            step: 1.0,
                            unit: NumericUnit::Celsius,
                        }),
                        Capability::HvacModes(vec![HvacMode::Cool, HvacMode::Heat]),
                    ]),
                );
                report(
                    &service,
                    &id,
                    [
                        (Property::Power, PropertyValue::Power(true)),
                        (Property::HvacMode, PropertyValue::HvacMode(HvacMode::Cool)),
                        (
                            Property::TargetTemperature,
                            PropertyValue::Temperature(24.0),
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
                connect(&server, 123456, 445566, 73);
                connect(&client, 445566, 123456, 73);
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
                    panic!(
                        "Matter service loop stopped before the thermostat controller completed"
                    );
                };
                let controller = async {
                    let subscription = subscribe_thermostat_cooling(&client, endpoint)
                        .await
                        .unwrap();
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
                            Property::TargetTemperature,
                            PropertyValue::Temperature(25.0),
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
                                .attrs::<i16>(
                                    thermostat::FULL_CLUSTER.id,
                                    thermostat::AttributeId::OccupiedCoolingSetpoint as _,
                                )
                                .map(|(_, value)| value.unwrap())
                                .collect::<Vec<_>>(),
                            [2500]
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
                            Property::TargetTemperature,
                            PropertyValue::Temperature(25.0),
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
                    assert!(
                        !duplicate,
                        "same thermostat value produced a second Matter report"
                    );
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for the thermostat subscription");
                    }),
                )
                .await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
