use super::*;

#[test]
fn real_vacuum_declares_rvc_clusters_and_ancillary_battery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let descriptor = compile_spec(
        "xiaomi.vacuum.c104",
        include_str!("../../../../tests/fixtures/miot_specs/xiaomi.vacuum.c104.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.definition.role == FeatureRole::Vacuum)
    .unwrap();
    let mut id = feature(FeatureRole::Vacuum);
    id.service_instance = descriptor.definition.service_instance;
    service.publish(
        id.clone(),
        descriptor.definition.name,
        descriptor.definition.capabilities,
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;

    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        let vacuum = node.endpoint(endpoint).expect("RVC endpoint");
        assert!(vacuum.device_types.iter().any(|item| item.dtype == 0x0074));
        for cluster in [
            3,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_operational_state::FULL_CLUSTER.id,
            power_source::FULL_CLUSTER.id,
            57,
        ] {
            assert!(
                vacuum.cluster(cluster).is_some(),
                "missing cluster {cluster:#x}"
            );
        }
        assert!(vacuum.cluster(on_off::FULL_CLUSTER.id).is_none());
        let operational = vacuum
            .cluster(rvc_operational_state::FULL_CLUSTER.id)
            .unwrap();
        for attribute in [
            rvc_operational_state::AttributeId::PhaseList,
            rvc_operational_state::AttributeId::CurrentPhase,
            rvc_operational_state::AttributeId::OperationalStateList,
            rvc_operational_state::AttributeId::OperationalState,
            rvc_operational_state::AttributeId::OperationalError,
        ] {
            assert!(operational.attribute(attribute as _).is_some());
        }
        assert!(
            operational
                .attribute(rvc_operational_state::AttributeId::CountdownTime as _)
                .is_none()
        );
        assert!(
            operational
                .command(rvc_operational_state::CommandId::GoHome as _)
                .is_some()
        );
        for command in [
            rvc_operational_state::CommandId::Pause,
            rvc_operational_state::CommandId::Resume,
        ] {
            assert!(operational.command(command as _).is_none());
        }
        assert!(
            operational
                .event(rvc_operational_state::EventId::OperationalError as _)
                .is_some()
        );
        assert!(
            operational
                .event(rvc_operational_state::EventId::OperationCompletion as _)
                .is_none()
        );
    });
}

#[test]
fn vacuum_without_dock_capability_omits_go_home() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::Vacuum);
    service.publish(
        id.clone(),
        "Vacuum",
        FeatureCapabilities(vec![Capability::VacuumControl]),
    );
    let endpoint = store.devices().allocate_feature(&id).unwrap().endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        let operational = node
            .endpoint(endpoint)
            .unwrap()
            .cluster(rvc_operational_state::FULL_CLUSTER.id)
            .unwrap();
        assert!(
            operational
                .command(rvc_operational_state::CommandId::GoHome as _)
                .is_none()
        );
    });
}

#[test]
fn rvc_tlv_uses_real_modes_and_keeps_accepted_commands_unconfirmed() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "xiaomi.vacuum.c104",
            include_str!("../../../../tests/fixtures/miot_specs/xiaomi.vacuum.c104.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Vacuum)
        .unwrap();
        let mut id = feature(FeatureRole::Vacuum);
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
                    Property::VacuumOperationalState,
                    PropertyValue::VacuumOperationalState(
                        crate::device::VacuumOperationalState::Idle,
                    ),
                ),
                (
                    Property::VacuumCleanMode,
                    PropertyValue::VacuumCleanMode(crate::device::VacuumCleanMode::Vacuum),
                ),
                (
                    Property::VacuumFault,
                    PropertyValue::VacuumFault("0".into()),
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

        let supported = Context::new_at(
            &im,
            endpoint,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_run_mode::AttributeId::SupportedModes as _,
        )
        .read_tlv(&model)
        .await;
        let modes = TLVArray::<globals::ModeOptionStruct<'_>>::from_tlv(&value_element(&supported))
            .unwrap()
            .iter()
            .map(|mode| {
                let mode = mode.unwrap();
                (
                    mode.mode().unwrap(),
                    mode.mode_tags()
                        .unwrap()
                        .iter()
                        .map(|tag| tag.unwrap().value().unwrap())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            modes,
            vec![
                (0, vec![rvc_run_mode::ModeTag::Idle as u16]),
                (1, vec![rvc_run_mode::ModeTag::Cleaning as u16]),
            ]
        );
        let supported = Context::new_at(
            &im,
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::AttributeId::SupportedModes as _,
        )
        .read_tlv(&model)
        .await;
        let modes = TLVArray::<globals::ModeOptionStruct<'_>>::from_tlv(&value_element(&supported))
            .unwrap()
            .iter()
            .map(|mode| {
                let mode = mode.unwrap();
                (
                    mode.mode().unwrap(),
                    mode.mode_tags()
                        .unwrap()
                        .iter()
                        .map(|tag| tag.unwrap().value().unwrap())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            modes,
            vec![
                (0, vec![rvc_clean_mode::ModeTag::Vacuum as u16]),
                (
                    1,
                    vec![
                        rvc_clean_mode::ModeTag::Vacuum as u16,
                        rvc_clean_mode::ModeTag::Mop as u16,
                    ],
                ),
                (2, vec![rvc_clean_mode::ModeTag::Mop as u16]),
            ]
        );

        let change = command_data(|writer| writer.u8(&TLVTag::Context(0), 1).unwrap());
        let change = Context::command_at(
            &im,
            endpoint,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_run_mode::CommandId::ChangeToMode as _,
            &change,
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &change,
                InvokeReplyInstance::new(change.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        assert_eq!(
            rvc_run_mode::ChangeToModeResponse::from_tlv(&command_response_element(&reply))
                .unwrap()
                .status()
                .unwrap(),
            0
        );
        assert_eq!(
            commands.0.borrow().as_slice(),
            [(id.clone(), vec![DeviceCommand::StartVacuum])]
        );
        let current = Context::new_at(
            &im,
            endpoint,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_run_mode::AttributeId::CurrentMode as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(value_element(&current).u8().unwrap(), 0);

        let clean = command_data(|writer| writer.u8(&TLVTag::Context(0), 1).unwrap());
        let clean = Context::command_at(
            &im,
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::CommandId::ChangeToMode as _,
            &clean,
        );
        model
            .invoke(
                &clean,
                InvokeReplyInstance::new(clean.cmd(), WriteBuf::new(&mut [0; 128])),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow()[1],
            (
                id.clone(),
                vec![DeviceCommand::SetVacuumCleanMode(
                    crate::device::VacuumCleanMode::VacuumAndMop
                )]
            )
        );
        let current = Context::new_at(
            &im,
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::AttributeId::CurrentMode as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(value_element(&current).u8().unwrap(), 0);

        let invalid = command_data(|writer| writer.u8(&TLVTag::Context(0), 99).unwrap());
        let invalid = Context::command_at(
            &im,
            endpoint,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_run_mode::CommandId::ChangeToMode as _,
            &invalid,
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &invalid,
                InvokeReplyInstance::new(invalid.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        let response =
            rvc_run_mode::ChangeToModeResponse::from_tlv(&command_response_element(&reply))
                .unwrap();
        assert_eq!(response.status().unwrap(), 1);
        assert!(response.status_text().unwrap().is_some());
        assert_eq!(commands.0.borrow().len(), 2);

        let malformed = command_data(|_| {});
        let malformed = Context::command_at(
            &im,
            endpoint,
            rvc_run_mode::FULL_CLUSTER.id,
            rvc_run_mode::CommandId::ChangeToMode as _,
            &malformed,
        );
        assert!(
            model
                .invoke(
                    &malformed,
                    InvokeReplyInstance::new(malformed.cmd(), WriteBuf::new(&mut [0; 128])),
                )
                .await
                .is_err()
        );
        assert_eq!(commands.0.borrow().len(), 2);

        let go_home_data = command_data(|_| {});
        let go_home = Context::command_at(
            &im,
            endpoint,
            rvc_operational_state::FULL_CLUSTER.id,
            rvc_operational_state::CommandId::GoHome as _,
            &go_home_data,
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &go_home,
                InvokeReplyInstance::new(go_home.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        let response = rvc_operational_state::OperationalCommandResponse::from_tlv(
            &command_response_element(&reply),
        )
        .unwrap();
        assert_eq!(
            response
                .command_response_state()
                .unwrap()
                .error_state_id()
                .unwrap(),
            rvc_operational_state::ErrorStateEnum::NoError as u8
        );
        assert_eq!(
            commands.0.borrow()[2],
            (id.clone(), vec![DeviceCommand::ReturnVacuumToDock])
        );

        report(
            &service,
            &id,
            [(
                Property::VacuumOperationalState,
                PropertyValue::VacuumOperationalState(
                    crate::device::VacuumOperationalState::Returning,
                ),
            )],
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &go_home,
                InvokeReplyInstance::new(go_home.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow().len(),
            3,
            "seeking sent another dock action"
        );
        let response = rvc_operational_state::OperationalCommandResponse::from_tlv(
            &command_response_element(&reply),
        )
        .unwrap();
        assert_eq!(
            response
                .command_response_state()
                .unwrap()
                .error_state_id()
                .unwrap(),
            rvc_operational_state::ErrorStateEnum::NoError as u8
        );

        report(
            &service,
            &id,
            [
                (
                    Property::VacuumOperationalState,
                    PropertyValue::VacuumOperationalState(
                        crate::device::VacuumOperationalState::Cleaning,
                    ),
                ),
                (
                    Property::VacuumFault,
                    PropertyValue::VacuumFault("17".into()),
                ),
            ],
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &go_home,
                InvokeReplyInstance::new(go_home.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        assert_eq!(
            commands.0.borrow().len(),
            3,
            "fault dispatched a dock action"
        );
        let response = rvc_operational_state::OperationalCommandResponse::from_tlv(
            &command_response_element(&reply),
        )
        .unwrap();
        assert_eq!(
            response
                .command_response_state()
                .unwrap()
                .error_state_id()
                .unwrap(),
            rvc_operational_state::ErrorStateEnum::CommandInvalidInState as u8
        );

        let state = Context::new_at(
            &im,
            endpoint,
            rvc_operational_state::FULL_CLUSTER.id,
            rvc_operational_state::AttributeId::OperationalState as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(
            value_element(&state).u8().unwrap(),
            rvc_operational_state::OperationalStateEnum::VError as u8
        );

        service.apply_unknown(&id, Property::VacuumFault, service.next_report_version());
        let state = Context::new_at(
            &im,
            endpoint,
            rvc_operational_state::FULL_CLUSTER.id,
            rvc_operational_state::AttributeId::OperationalState as _,
        )
        .read_tlv(&model)
        .await;
        assert_eq!(
            value_element(&state).u8().unwrap(),
            rvc_operational_state::OperationalStateEnum::Running as u8
        );
        assert!(
            Context::new_at(
                &im,
                endpoint,
                rvc_operational_state::FULL_CLUSTER.id,
                rvc_operational_state::AttributeId::OperationalError as _,
            )
            .read_tlv_result(&model)
            .await
            .is_err()
        );
        let mut reply = [0; 128];
        model
            .invoke(
                &go_home,
                InvokeReplyInstance::new(go_home.cmd(), WriteBuf::new(&mut reply)),
            )
            .await
            .unwrap();
        assert_eq!(commands.0.borrow().len(), 4);
        assert_eq!(
            commands.0.borrow()[3],
            (id, vec![DeviceCommand::ReturnVacuumToDock])
        );
    });
}

#[test]
fn rvc_commands_reach_runtime_with_c104_wire_operations() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "xiaomi.vacuum.c104",
            include_str!("../../../../tests/fixtures/miot_specs/xiaomi.vacuum.c104.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.definition.role == FeatureRole::Vacuum)
        .unwrap();
        let mut id = feature(FeatureRole::Vacuum);
        id.physical.parent_did = DeviceDid::new("xiaomi.vacuum.c104").unwrap();
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
            [(
                Property::VacuumOperationalState,
                PropertyValue::VacuumOperationalState(crate::device::VacuumOperationalState::Idle),
            )],
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

        for mode in [1, 0] {
            let data = command_data(|writer| writer.u8(&TLVTag::Context(0), mode).unwrap());
            let command = Context::command_at(
                &im,
                endpoint,
                rvc_run_mode::FULL_CLUSTER.id,
                rvc_run_mode::CommandId::ChangeToMode as _,
                &data,
            );
            let (result, ()) = zip(
                model.invoke(
                    &command,
                    InvokeReplyInstance::new(command.cmd(), WriteBuf::new(&mut [0; 128])),
                ),
                runtime.run_until_idle(),
            )
            .await;
            result.unwrap();
        }
        let data = command_data(|writer| writer.u8(&TLVTag::Context(0), 2).unwrap());
        let command = Context::command_at(
            &im,
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::CommandId::ChangeToMode as _,
            &data,
        );
        let (result, ()) = zip(
            model.invoke(
                &command,
                InvokeReplyInstance::new(command.cmd(), WriteBuf::new(&mut [0; 128])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();
        let data = command_data(|_| {});
        let command = Context::command_at(
            &im,
            endpoint,
            rvc_operational_state::FULL_CLUSTER.id,
            rvc_operational_state::CommandId::GoHome as _,
            &data,
        );
        let (result, ()) = zip(
            model.invoke(
                &command,
                InvokeReplyInstance::new(command.cmd(), WriteBuf::new(&mut [0; 128])),
            ),
            runtime.run_until_idle(),
        )
        .await;
        result.unwrap();

        assert_eq!(
            calls.borrow().as_slice(),
            [
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::StartVacuum,
                    operation: WireOperation::InvokeAction {
                        siid: 2,
                        aiid: 1,
                        input: vec![],
                    },
                },
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::StopVacuum,
                    operation: WireOperation::InvokeAction {
                        siid: 2,
                        aiid: 2,
                        input: vec![],
                    },
                },
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::SetVacuumCleanMode(crate::device::VacuumCleanMode::Mop,),
                    operation: WireOperation::SetProperty {
                        siid: 2,
                        piid: 4,
                        value: WireValue::Integer(2),
                    },
                },
                TransportCommand {
                    device: id.physical.clone(),
                    typed: DeviceCommand::ReturnVacuumToDock,
                    operation: WireOperation::InvokeAction {
                        siid: 3,
                        aiid: 1,
                        input: vec![],
                    },
                },
            ]
        );
    });
}

#[test]
fn reconcile_refreshes_rvc_modes_without_rebuilding_the_endpoint() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::Vacuum);
        let capabilities = |modes| {
            FeatureCapabilities(vec![
                Capability::VacuumControl,
                Capability::VacuumDock,
                Capability::VacuumCleanModes(modes),
            ])
        };
        service.publish(
            id.clone(),
            "Vacuum",
            capabilities(vec![crate::device::VacuumCleanMode::Vacuum]),
        );
        report(
            &service,
            &id,
            [(
                Property::VacuumCleanMode,
                PropertyValue::VacuumCleanMode(crate::device::VacuumCleanMode::Vacuum),
            )],
        );
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
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::AttributeId::SupportedModes as _,
        );
        let mut run = std::pin::pin!(model.run(&ctx));
        assert!(poll_once(&mut run).await.is_none());

        service.publish(
            id.clone(),
            "Vacuum",
            capabilities(vec![crate::device::VacuumCleanMode::Mop]),
        );
        assert!(poll_once(&mut run).await.is_none());
        assert_eq!(model.endpoint_for(&id), Some(endpoint));
        assert!(!model.take_rebuild_request());
        assert!(ctx.has_change(
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::AttributeId::SupportedModes as _,
        ));
        assert!(ctx.has_change(
            endpoint,
            rvc_clean_mode::FULL_CLUSTER.id,
            rvc_clean_mode::AttributeId::CurrentMode as _,
        ));
        assert!(
            Context::new_at(
                &im,
                endpoint,
                rvc_clean_mode::FULL_CLUSTER.id,
                rvc_clean_mode::AttributeId::CurrentMode as _,
            )
            .read_tlv_result(&model)
            .await
            .is_err(),
            "the old confirmed mode is not part of the refreshed supported list"
        );
        let supported = ctx.read_tlv(&model).await;
        let modes = TLVArray::<globals::ModeOptionStruct<'_>>::from_tlv(&value_element(&supported))
            .unwrap()
            .iter()
            .map(|mode| mode.unwrap().mode().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(modes, [2]);
    });
}

#[test]
fn real_im_rvc_fault_subscription_emits_each_new_confirmed_fault_once() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let store = Store::open(directory.path()).unwrap();
                let identity = store.load_identity().unwrap();
                let service = DeviceService::new();
                let descriptor = compile_spec(
                    "xiaomi.vacuum.c104",
                    include_str!("../../../../tests/fixtures/miot_specs/xiaomi.vacuum.c104.json"),
                )
                .unwrap()
                .features
                .into_iter()
                .find(|feature| feature.definition.role == FeatureRole::Vacuum)
                .unwrap();
                let mut id = feature(FeatureRole::Vacuum);
                id.service_instance = descriptor.definition.service_instance;
                service.publish(
                    id.clone(),
                    descriptor.definition.name,
                    descriptor.definition.capabilities,
                );
                report(
                    &service,
                    &id,
                    [(
                        Property::VacuumFault,
                        PropertyValue::VacuumFault("17".into()),
                    )],
                );
                service.apply_unknown(&id, Property::VacuumFault, service.next_report_version());
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
                    panic!("Matter service loop stopped before the controller completed");
                };
                let controller = async {
                    async_io::Timer::after(Duration::from_millis(10)).await;
                    let subscription = subscribe_rvc_fault(&client, endpoint).await.unwrap();
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
                            Property::VacuumFault,
                            PropertyValue::VacuumFault("17".into()),
                        )],
                    );
                    let replay = or(
                        async { Exchange::accept(&client).await.map(|_| true) },
                        async {
                            async_io::Timer::after(Duration::from_millis(100)).await;
                            Ok(false)
                        },
                    )
                    .await
                    .unwrap();
                    assert!(!replay, "the initial confirmed fault was replayed");

                    for fault in [18, 19] {
                        report(
                            &service,
                            &id,
                            [(
                                Property::VacuumFault,
                                PropertyValue::VacuumFault(fault.to_string()),
                            )],
                        );
                        let mut exchange = Exchange::accept(&client).await.unwrap();
                        exchange.recv_fetch().await.unwrap();
                        {
                            let rx = exchange.rx().unwrap();
                            let report =
                                ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                            assert_eq!(report.subscription_id, Some(subscription));
                            let events = report
                                .event_reports
                                .as_ref()
                                .unwrap()
                                .iter()
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap();
                            assert_eq!(events.len(), 1);
                            let rs_matter::im::EventResp::Data(event) = &events[0] else {
                                panic!("fault report was not event data")
                            };
                            assert_eq!(
                                event.path.to_gp(),
                                GenericPath::new(
                                    Some(endpoint),
                                    Some(rvc_operational_state::FULL_CLUSTER.id),
                                    Some(rvc_operational_state::EventId::OperationalError as _),
                                )
                            );
                            let event =
                                rvc_operational_state::OperationalError::from_tlv(&event.data)
                                    .unwrap();
                            let error = event.error_state().unwrap();
                            assert_eq!(error.error_state_id().unwrap(), 0x80);
                            assert_eq!(
                                error.error_state_details().unwrap(),
                                Some(format!("MIoT fault {fault}").as_str())
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

                        if fault == 18 {
                            report(
                                &service,
                                &id,
                                [(
                                    Property::VacuumFault,
                                    PropertyValue::VacuumFault("18".into()),
                                )],
                            );
                            service.apply_unknown(
                                &id,
                                Property::VacuumFault,
                                service.next_report_version(),
                            );
                            report(
                                &service,
                                &id,
                                [(
                                    Property::VacuumFault,
                                    PropertyValue::VacuumFault("18".into()),
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
                            assert!(!duplicate, "the same fault was emitted again");
                        }
                    }

                    report(
                        &service,
                        &id,
                        [(
                            Property::VacuumFault,
                            PropertyValue::VacuumFault("0".into()),
                        )],
                    );
                    let clear_event = or(
                        async { Exchange::accept(&client).await.map(|_| true) },
                        async {
                            async_io::Timer::after(Duration::from_millis(100)).await;
                            Ok(false)
                        },
                    )
                    .await
                    .unwrap();
                    assert!(!clear_event, "clearing a fault emitted another fault event");

                    for index in 0..300 {
                        report(
                            &service,
                            &id,
                            [(
                                Property::Battery,
                                PropertyValue::Percent(
                                    Percent::new(if index % 2 == 0 { 40.0 } else { 41.0 }).unwrap(),
                                ),
                            )],
                        );
                    }
                    report(
                        &service,
                        &id,
                        [(
                            Property::VacuumFault,
                            PropertyValue::VacuumFault("20".into()),
                        )],
                    );
                    let mut exchange = Exchange::accept(&client).await.unwrap();
                    exchange.recv_fetch().await.unwrap();
                    let rx = exchange.rx().unwrap();
                    let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                    let events = report
                        .event_reports
                        .as_ref()
                        .unwrap()
                        .iter()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap();
                    assert!(events.iter().any(|event| {
                        let rs_matter::im::EventResp::Data(event) = event else {
                            return false;
                        };
                        let Ok(event) =
                            rvc_operational_state::OperationalError::from_tlv(&event.data)
                        else {
                            return false;
                        };
                        let Ok(error) = event.error_state() else {
                            return false;
                        };
                        error.error_state_details().ok() == Some(Some("MIoT fault 20"))
                    }));
                };
                or(
                    services,
                    or(controller, async {
                        async_io::Timer::after(Duration::from_secs(5)).await;
                        panic!("timed out waiting for RVC fault events");
                    }),
                )
                .await;
            })
        })
        .unwrap()
        .join()
        .unwrap();
}
