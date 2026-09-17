use super::super::{
    DeviceBridgeModel,
    device_bridge::config_signature,
    lighting::{LightingHandler, SceneOnOff, rgb_to_xy},
    sensors,
};
use super::handlers::Context;
use super::subscriptions::{Pipe, ReceivePipe, SendPipe, connect, connect_at};
use crate::{
    device::{
        AccountId, Capability, CommandOutcome, DeviceCommand, DeviceCommandSink, DeviceDid,
        DeviceService, FeatureCapabilities, FeatureIdentity, FeatureRole, HomeId, HvacMode,
        NumericRange, NumericUnit, Percent, PhysicalDeviceId, Property, PropertyValue, RgbColor,
        SensingModality, StateReport, StateSource,
    },
    storage::{MatterStore, Store},
    xiaomi::{
        catalog::{
            LegacyMiioOperation, Mcn02LegacyMapping, WireOperation, WireValue, compile_spec,
        },
        runtime::{
            CommandRuntime, CommandTransport, ControlPath, OperationPaths, RuntimeFeature,
            SendGuard, TransportCommand, TransportFailure,
        },
    },
};
use event_listener::Event;
use futures_lite::future::{block_on, or, poll_once, zip};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::test_only_crypto,
    dm::{
        AsyncHandler, InvokeContext, InvokeReplyInstance, Metadata,
        clusters::{
            app::color_control::{RgbGamma, SetDeviceColor},
            decl::{
                color_control, fan_control, groups, level_control, occupancy_sensing, on_off,
                power_source, scenes_management, temperature_measurement, thermostat,
            },
            scenes::{AttributeValuePairStruct, SceneClusterHandler},
        },
        devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET},
        endpoints,
        networks::{SysNetifs, eth::EthNetwork},
    },
    error::{Error, ErrorCode},
    fabric::GroupKeyMapping,
    im::{
        AttrDataTag, AttrPath, CmdDataTag, EthInteractionModelState, EventPath, GenericPath,
        IMStatusCode, InteractionModel, OpCode, StatusResp,
        client::{ImClient, SubscribeOutcome, TxOutcome},
        encoding::ReportDataResp,
    },
    respond::DefaultResponder,
    tlv::{FromTLV, Nullable, TLVArray, TLVElement, TLVTag, TLVWrite, ToTLV, Utf8Str},
    transport::{
        exchange::{Exchange, MatterBuffers},
        network::NoNetwork,
    },
    utils::storage::WriteBuf,
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    num::NonZeroU8,
    rc::Rc,
    time::Duration,
};

#[derive(Default)]
struct RecordingCommands(RefCell<Vec<(FeatureIdentity, Vec<DeviceCommand>)>>);

impl DeviceCommandSink for RecordingCommands {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> futures_util::future::LocalBoxFuture<'static, CommandOutcome> {
        self.0.borrow_mut().push((feature, commands));
        Box::pin(async { CommandOutcome::Accepted })
    }

    fn stop_adjustment(&self, _feature: &FeatureIdentity, _property: Property) {}
}

struct CapturingTransport(Rc<RefCell<Vec<TransportCommand>>>);

impl CommandTransport for CapturingTransport {
    fn available_paths(&self, _device: &PhysicalDeviceId) -> OperationPaths {
        OperationPaths {
            gateway: true,
            ..OperationPaths::default()
        }
    }

    fn send(
        &self,
        _path: ControlPath,
        command: TransportCommand,
        _timeout: Duration,
        guard: SendGuard,
    ) -> futures_util::future::LocalBoxFuture<'static, Result<(), TransportFailure>> {
        let calls = self.0.clone();
        Box::pin(async move {
            if !guard.permitted() {
                return Err(TransportFailure::Unavailable);
            }
            guard.shared_state().mark_sent();
            calls.borrow_mut().push(command);
            Ok(())
        })
    }
}

struct ControlledCommands {
    calls: RefCell<Vec<(FeatureIdentity, Vec<DeviceCommand>)>>,
    outcomes: RefCell<VecDeque<CommandOutcome>>,
    released: Rc<Cell<usize>>,
    wake: Rc<Event>,
}

impl ControlledCommands {
    fn new(outcomes: impl IntoIterator<Item = CommandOutcome>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            outcomes: RefCell::new(outcomes.into_iter().collect()),
            released: Rc::new(Cell::new(0)),
            wake: Rc::new(Event::new()),
        }
    }

    fn release_next(&self) {
        self.released.set(self.released.get() + 1);
        self.wake.notify(usize::MAX);
    }
}

impl DeviceCommandSink for ControlledCommands {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> futures_util::future::LocalBoxFuture<'static, CommandOutcome> {
        let index = self.calls.borrow().len();
        self.calls.borrow_mut().push((feature, commands));
        let outcome = self
            .outcomes
            .borrow_mut()
            .pop_front()
            .unwrap_or(CommandOutcome::Accepted);
        let released = self.released.clone();
        let wake = self.wake.clone();
        Box::pin(async move {
            while released.get() <= index {
                let listener = wake.listen();
                if released.get() <= index {
                    listener.await;
                }
            }
            outcome
        })
    }

    fn stop_adjustment(&self, _feature: &FeatureIdentity, _property: Property) {}
}

fn feature(role: FeatureRole) -> FeatureIdentity {
    FeatureIdentity {
        physical: PhysicalDeviceId {
            account: AccountId::new("u").unwrap(),
            home: HomeId::new("h").unwrap(),
            parent_did: DeviceDid::new("d").unwrap(),
        },
        service_instance: 2,
        role,
    }
}

fn configuration_version(store: &MatterStore) -> u32 {
    let bytes = store
        .get(rs_matter::persist::BASIC_INFO_KEY)
        .unwrap()
        .unwrap();
    rs_matter::dm::clusters::basic_info::BasicInfoSettings::from_tlv(&TLVElement::new(&bytes))
        .unwrap()
        .configuration_version
}

#[test]
fn empty_bridge_has_only_root_and_aggregator() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let model =
        DeviceBridgeModel::new(DeviceService::new(), store.devices(), store.matter()).unwrap();
    model.access(|node| {
        assert_eq!(
            node.endpoints
                .iter()
                .map(|endpoint| endpoint.id)
                .collect::<Vec<_>>(),
            [0, 1]
        )
    });
}

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
fn discrete_fan_declares_only_the_real_fan_shape() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let descriptor = compile_spec(
        "dmaker.fan.p5c",
        include_str!("../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.role == FeatureRole::Fan)
    .unwrap();
    let mut id = feature(FeatureRole::Fan);
    id.service_instance = descriptor.service_instance;
    service.publish(id.clone(), descriptor.name, descriptor.capabilities);
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
            include_str!("../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::Fan)
        .unwrap();
        let mut id = feature(FeatureRole::Fan);
        id.service_instance = descriptor.service_instance;
        service.publish(id.clone(), descriptor.name, descriptor.capabilities);
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
            include_str!("../../../tests/fixtures/miot_specs/yeelink.bhf_light.v13.json"),
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
                .find(|feature| feature.role == role)
                .unwrap();
            let mut id = feature(role);
            id.service_instance = descriptor.service_instance;
            service.publish(
                id.clone(),
                descriptor.name.clone(),
                descriptor.capabilities.clone(),
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
            include_str!("../../../tests/fixtures/miot_specs/dmaker.fan.p5c.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::Fan)
        .unwrap();
        let mut id = feature(FeatureRole::Fan);
        id.service_instance = descriptor.service_instance;
        service.publish(
            id.clone(),
            descriptor.name.clone(),
            descriptor.capabilities.clone(),
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
fn matter_light_commands_reach_real_runtime_with_light3_wire_values() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let descriptor = compile_spec(
            "yeelink.light.light3",
            include_str!("../../../tests/fixtures/miot_specs/yeelink.light.light3.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::Light)
        .unwrap();
        let mut id = feature(FeatureRole::Light);
        id.service_instance = descriptor.service_instance;
        service.publish(
            id.clone(),
            descriptor.name.clone(),
            descriptor.capabilities.clone(),
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
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
                    .kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&restored_server, &restored_kv, true)
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

#[test]
fn presence_and_illuminance_use_distinct_simple_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::OccupancySensor);
    let illuminance = feature(FeatureRole::IlluminanceSensor);
    service.publish(
        id.clone(),
        "Hall presence",
        FeatureCapabilities(vec![
            Capability::Occupancy,
            Capability::SensingModalities(vec![SensingModality::Pir, SensingModality::Radar]),
        ]),
    );
    service.publish(
        illuminance.clone(),
        "Hall illuminance",
        FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
            minimum: 0.0,
            maximum: 10_000.0,
            step: 1.0,
            unit: NumericUnit::Lux,
        })]),
    );
    let allocation = store.devices().allocate_feature(&id).unwrap();
    let illuminance_allocation = store.devices().allocate_feature(&illuminance).unwrap();
    let public_id = allocation.public_id.clone();
    let endpoint = allocation.endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();

    model.access(|node| {
        let sensor = node.endpoint(endpoint).unwrap();
        assert_eq!(sensor.unique_id, Some(public_id.as_str()));
        assert!(sensor.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_none());
        let occupancy = sensor.cluster(occupancy_sensing::FULL_CLUSTER.id).unwrap();
        assert_eq!(
            occupancy.feature_map,
            (occupancy_sensing::Feature::PASSIVE_INFRARED | occupancy_sensing::Feature::RADAR)
                .bits()
        );
        let illuminance = node.endpoint(illuminance_allocation.endpoint).unwrap();
        assert!(
            illuminance
                .cluster(sensors::ILLUMINANCE_CLUSTER.id)
                .is_some()
        );
        assert!(
            illuminance
                .cluster(occupancy_sensing::FULL_CLUSTER.id)
                .is_none()
        );
    });
}

#[test]
fn lux_conversion_uses_matter_logarithmic_encoding_without_fake_zero() {
    assert_eq!(sensors::matter_lux(0.0), Some(0));
    assert_eq!(sensors::matter_lux(0.1), Some(0));
    assert_eq!(sensors::matter_lux(1.0), Some(1));
    assert_eq!(sensors::matter_lux(100.0), Some(20_001));
    assert_eq!(sensors::matter_lux(-1.0), None);
    assert_eq!(sensors::matter_lux(f64::NAN), None);
}

fn report(
    service: &DeviceService,
    id: &FeatureIdentity,
    values: impl IntoIterator<Item = (Property, PropertyValue)>,
) {
    service.apply_report(StateReport::new(
        id.clone(),
        service.next_report_version(),
        StateSource::Lan,
        1,
        values,
    ));
}

fn value_element(bytes: &[u8]) -> TLVElement<'_> {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(1)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(2)
        .unwrap()
}

fn command_data(write: impl FnOnce(&mut WriteBuf<'_>)) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    let mut writer = WriteBuf::new(&mut bytes);
    writer.start_struct(&TLVTag::Anonymous).unwrap();
    write(&mut writer);
    writer.end_container().unwrap();
    writer.as_slice().to_vec()
}

fn scalar_data(write: impl FnOnce(&mut WriteBuf<'_>)) -> Vec<u8> {
    let mut bytes = vec![0; 16];
    let mut writer = WriteBuf::new(&mut bytes);
    write(&mut writer);
    writer.as_slice().to_vec()
}

#[test]
fn sensor_tlv_reads_preserve_fractional_values_unknowns_and_contact_polarity() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let service = DeviceService::new();
        let humidity = feature(FeatureRole::HumiditySensor);
        let presence = feature(FeatureRole::OccupancySensor);
        let illuminance = feature(FeatureRole::IlluminanceSensor);
        let contact = feature(FeatureRole::ContactSensor);
        service.publish(
            humidity.clone(),
            "Humidity",
            FeatureCapabilities(vec![Capability::Humidity(NumericRange {
                minimum: 0.0,
                maximum: 100.0,
                step: 0.1,
                unit: NumericUnit::Percent,
            })]),
        );
        service.publish(
            presence.clone(),
            "Presence",
            FeatureCapabilities(vec![
                Capability::Occupancy,
                Capability::SensingModalities(vec![SensingModality::Radar]),
            ]),
        );
        service.publish(
            illuminance.clone(),
            "Illuminance",
            FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
                minimum: 0.0,
                maximum: 10_000.0,
                step: 0.1,
                unit: NumericUnit::Lux,
            })]),
        );
        service.publish(
            contact.clone(),
            "Door",
            FeatureCapabilities(vec![Capability::Contact]),
        );
        let humidity_ep = store
            .devices()
            .allocate_feature(&humidity)
            .unwrap()
            .endpoint;
        let presence_ep = store
            .devices()
            .allocate_feature(&presence)
            .unwrap()
            .endpoint;
        let illuminance_ep = store
            .devices()
            .allocate_feature(&illuminance)
            .unwrap()
            .endpoint;
        let contact_ep = store.devices().allocate_feature(&contact).unwrap().endpoint;
        report(
            &service,
            &humidity,
            [(
                Property::Humidity,
                PropertyValue::Percent(Percent::new(55.5).unwrap()),
            )],
        );
        report(
            &service,
            &presence,
            [
                (
                    Property::Occupancy,
                    PropertyValue::Occupancy(crate::device::PresenceState::Occupied),
                ),
                (Property::Motion, PropertyValue::Motion(true)),
            ],
        );
        report(
            &service,
            &illuminance,
            [(Property::Illuminance, PropertyValue::Illuminance(0.1))],
        );
        report(
            &service,
            &contact,
            [(Property::Contact, PropertyValue::ContactOpen(true))],
        );
        let model =
            DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        let identity = store.load_identity().unwrap();
        let info = super::super::basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let crypto = test_only_crypto();
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        let mut random = rand::rng();
        let handler = endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(&mut random)
            .chain(|endpoint, _| endpoint != 0, &model);
        let im = InteractionModel::new(&matter, &crypto, &buffers, (&model, &handler), &kv, &state);

        let bytes = Context::new_at(&im, humidity_ep, 0x0405, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(5550)
        );
        let bytes = Context::new_at(&im, illuminance_ep, 0x0400, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(0)
        );
        let bytes = Context::new_at(&im, illuminance_ep, 0x0400, 1)
            .read_tlv(&model)
            .await;
        assert_eq!(
            Nullable::<u16>::from_tlv(&value_element(&bytes))
                .unwrap()
                .into_option(),
            Some(1)
        );
        let bytes = Context::new_at(&im, presence_ep, 0x0406, 0)
            .read_tlv(&model)
            .await;
        assert_eq!(
            occupancy_sensing::OccupancyBitmap::from_tlv(&value_element(&bytes)).unwrap(),
            occupancy_sensing::OccupancyBitmap::OCCUPIED
        );
        let bytes = Context::new_at(&im, contact_ep, 0x0045, 0)
            .read_tlv(&model)
            .await;
        assert!(!value_element(&bytes).bool().unwrap());

        service.apply_unknown(
            &presence,
            Property::Occupancy,
            service.next_report_version(),
        );
        assert!(
            Context::new_at(&im, presence_ep, 0x0406, 0)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
        service.apply_unknown(&contact, Property::Contact, service.next_report_version());
        assert!(
            Context::new_at(&im, contact_ep, 0x0045, 0)
                .read_tlv_result(&model)
                .await
                .is_err()
        );
    });
}

#[test]
fn battery_capability_adds_battery_power_source_shape_to_sensor_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let service = DeviceService::new();
    let id = feature(FeatureRole::TemperatureSensor);
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
    let allocation = store.devices().allocate_feature(&id).unwrap();
    let endpoint = allocation.endpoint;
    let model = DeviceBridgeModel::new(service, store.devices(), store.matter()).unwrap();
    model.access(|node| {
        assert!(node.endpoint(endpoint).unwrap().cluster(0x002f).is_some());
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
        include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
    )
    .unwrap()
    .features
    .into_iter()
    .find(|feature| feature.role == FeatureRole::Climate)
    .unwrap();
    let mut id = feature(FeatureRole::Climate);
    id.service_instance = descriptor.service_instance;
    service.publish(id.clone(), descriptor.name, descriptor.capabilities);
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
            include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::Climate)
        .unwrap();
        let mut id = feature(FeatureRole::Climate);
        id.service_instance = descriptor.service_instance;
        service.publish(id.clone(), descriptor.name, descriptor.capabilities);
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
            include_str!("../../../tests/fixtures/miot_specs/yeelink.bhf_light.v13.json"),
        )
        .unwrap()
        .features
        .into_iter()
        .find(|feature| feature.role == FeatureRole::BathHeaterClimate)
        .unwrap();
        let mut id = feature(FeatureRole::BathHeaterClimate);
        id.service_instance = descriptor.service_instance;
        service.publish(id.clone(), descriptor.name, descriptor.capabilities);
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
        let basic_info = super::super::basic_info(&identity);
        let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = test_only_crypto();
        let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
                include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn02.json"),
                2,
                1,
                3,
                3,
                1,
                2,
            ),
            (
                "lumi.acpartner.mcn04",
                include_str!("../../../tests/fixtures/miot_specs/lumi.acpartner.mcn04.json"),
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
                .find(|feature| feature.role == FeatureRole::Climate)
                .unwrap();
            let mut id = feature(FeatureRole::Climate);
            id.physical.parent_did = DeviceDid::new(model_name).unwrap();
            id.service_instance = descriptor.service_instance;
            service.publish(
                id.clone(),
                descriptor.name.clone(),
                descriptor.capabilities.clone(),
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
            let basic_info = super::super::basic_info(&identity);
            let matter = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
            let buffers: MatterBuffers = MatterBuffers::new();
            let state: EthInteractionModelState =
                EthInteractionModelState::new(EthNetwork::new_default());
            let crypto = test_only_crypto();
            let kv = matter.kv(super::super::storage::StoreAdapter::new(store.matter()));
            super::super::model::initialize_basic_info(&matter, &kv, true).unwrap();
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
fn visible_range_changes_configuration_but_native_step_does_not() {
    let capability = |minimum, maximum, step| {
        vec![Capability::Temperature(NumericRange {
            minimum,
            maximum,
            step,
            unit: NumericUnit::Celsius,
        })]
    };
    let original = config_signature("shape", &capability(-20.0, 60.0, 0.1));
    assert_eq!(
        original,
        config_signature("shape", &capability(-20.0, 60.0, 0.01))
    );
    assert_ne!(
        original,
        config_signature("shape", &capability(-30.0, 60.0, 0.1))
    );
}

#[test]
fn restart_restores_sensor_shapes_and_split_role_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    let service = DeviceService::new();
    let occupancy = feature(FeatureRole::OccupancySensor);
    let illuminance = feature(FeatureRole::IlluminanceSensor);
    service.restore(
        occupancy.clone(),
        "Presence",
        FeatureCapabilities(vec![
            Capability::Occupancy,
            Capability::SensingModalities(vec![SensingModality::Radar]),
            Capability::Battery(NumericRange {
                minimum: 0.0,
                maximum: 100.0,
                step: 1.0,
                unit: NumericUnit::Percent,
            }),
        ]),
    );
    service.restore(
        illuminance.clone(),
        "Illuminance",
        FeatureCapabilities(vec![Capability::Illuminance(NumericRange {
            minimum: 0.0,
            maximum: 10_000.0,
            step: 0.1,
            unit: NumericUnit::Lux,
        })]),
    );
    let store = Store::open(directory.path()).unwrap();
    let occupancy_endpoint = store
        .devices()
        .allocate_feature(&occupancy)
        .unwrap()
        .endpoint;
    let illuminance_endpoint = store
        .devices()
        .allocate_feature(&illuminance)
        .unwrap()
        .endpoint;
    let first = DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
    assert_eq!(first.endpoint_for(&occupancy), Some(occupancy_endpoint));
    assert_eq!(first.endpoint_for(&illuminance), Some(illuminance_endpoint));
    drop(first);
    drop(store);

    let reopened = Store::open(directory.path()).unwrap();
    let restored =
        DeviceBridgeModel::new(service.clone(), reopened.devices(), reopened.matter()).unwrap();
    assert!(!service.is_available(&occupancy));
    restored.access(|node| {
        let presence = node.endpoint(occupancy_endpoint).unwrap();
        assert!(
            presence
                .cluster(occupancy_sensing::FULL_CLUSTER.id)
                .is_some()
        );
        assert!(presence.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_none());
        assert!(presence.cluster(sensors::POWER_SOURCE_CLUSTER.id).is_some());
        let lux = node.endpoint(illuminance_endpoint).unwrap();
        assert!(lux.cluster(sensors::ILLUMINANCE_CLUSTER.id).is_some());
        assert!(lux.cluster(occupancy_sensing::FULL_CLUSTER.id).is_none());
    });
}

async fn subscribe_temperature(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(temperature_measurement::FULL_CLUSTER.id),
        Some(temperature_measurement::AttributeId::MeasuredValue as _),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let report = chunk.response()?;
        let values = report
            .attrs::<Nullable<i16>>(
                temperature_measurement::FULL_CLUSTER.id,
                temperature_measurement::AttributeId::MeasuredValue as _,
            )
            .map(|(_, value)| value.unwrap().into_option())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn subscribe_fan_percent(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(fan_control::FULL_CLUSTER.id),
        Some(fan_control::AttributeId::PercentCurrent as _),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let values = chunk
            .response()?
            .attrs::<u8>(
                fan_control::FULL_CLUSTER.id,
                fan_control::AttributeId::PercentCurrent as _,
            )
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn subscribe_thermostat_cooling(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(thermostat::FULL_CLUSTER.id),
        Some(thermostat::AttributeId::OccupiedCoolingSetpoint as _),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let values = chunk
            .response()?
            .attrs::<i16>(
                thermostat::FULL_CLUSTER.id,
                thermostat::AttributeId::OccupiedCoolingSetpoint as _,
            )
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
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
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
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
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
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

async fn subscribe_level(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(8),
        Some(level_control::AttributeId::CurrentLevel as _),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let values = chunk
            .response()?
            .attrs::<Nullable<u8>>(8, level_control::AttributeId::CurrentLevel as _)
            .map(|(_, value)| value.unwrap().into_option())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn invoke_identify(client: &Matter<'_>, endpoint: u16, seconds: u16) -> Result<(), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.invoke_sender(None).await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)?
                    .timed_request(false)?
                    .invoke_requests()?
                    .push()?
                    .path(endpoint, 3, 0)?
                    .data(|writer| {
                        writer.start_struct(&TLVTag::Context(CmdDataTag::Data as u8))?;
                        writer.u16(&TLVTag::Context(0), seconds)?;
                        writer.end_container()
                    })?
                    .end()?
                    .end()?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        if let Some(response) = chunk.response()?
            && let Some(items) = &response.invoke_responses
        {
            for item in items.iter() {
                match item? {
                    rs_matter::im::CmdResp::Status(status) => {
                        assert_eq!(status.status.status, IMStatusCode::Success);
                    }
                    rs_matter::im::CmdResp::Cmd(_) => {}
                }
            }
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(()),
        }
    }
}

async fn invoke_command(
    client: &Matter<'_>,
    fabric: NonZeroU8,
    endpoint: u16,
    cluster: u32,
    command: u32,
    data: &[u8],
) -> Result<(), Error> {
    let exchange = Exchange::initiate(client, test_only_crypto(), fabric, 123456).await?;
    let mut sender = exchange.invoke_sender(None).await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .suppress_response(false)?
                    .timed_request(false)?
                    .invoke_requests()?
                    .push()?
                    .path(endpoint, cluster, command)?
                    .data(|writer| {
                        TLVElement::new(data)
                            .to_tlv(&TLVTag::Context(CmdDataTag::Data as u8), writer)
                    })?
                    .end()?
                    .end()?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        if let Some(response) = chunk.response()?
            && let Some(items) = &response.invoke_responses
        {
            for item in items.iter() {
                match item? {
                    rs_matter::im::CmdResp::Status(status) => assert_eq!(
                        status.status.status,
                        IMStatusCode::Success,
                        "endpoint {endpoint} cluster {cluster:#x} command {command:#x}"
                    ),
                    rs_matter::im::CmdResp::Cmd(response) => {
                        if let Ok(status) = response.data.structure()?.ctx(0)?.u8() {
                            assert_eq!(
                                status, 0,
                                "endpoint {endpoint} cluster {cluster:#x} command {command:#x}"
                            );
                        }
                    }
                }
            }
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(()),
        }
    }
}

async fn scene_info(
    client: &Matter<'_>,
    fabric: NonZeroU8,
    endpoint: u16,
) -> Result<(u8, u8, u16, bool, u8), Error> {
    let exchange = Exchange::initiate(client, test_only_crypto(), fabric, 123456).await?;
    let mut sender = exchange.read_sender().await?;
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(scenes_management::FULL_CLUSTER.id),
        Some(scenes_management::AttributeId::FabricSceneInfo as _),
    ))];
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&paths)?
                    .fabric_filtered(true)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let report = chunk.response()?;
        if let Some((_, value)) = report
            .attrs::<rs_matter::tlv::TLVArray<'_, scenes_management::SceneInfoStruct<'_>>>(
                scenes_management::FULL_CLUSTER.id,
                scenes_management::AttributeId::FabricSceneInfo as _,
            )
            .next()
        {
            let value = value?;
            let mut rows = value.iter();
            let row = rows.next().ok_or(ErrorCode::InvalidData)??;
            return Ok((
                row.scene_count()?,
                row.current_scene()?.ok_or(ErrorCode::InvalidData)?,
                row.current_group()?.ok_or(ErrorCode::InvalidData)?,
                row.scene_valid()?.ok_or(ErrorCode::InvalidData)?,
                row.remaining_capacity()?,
            ));
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Err(ErrorCode::InvalidData.into()),
        }
    }
}

async fn read_wildcard_chunks(client: &Matter<'_>) -> Result<(usize, usize), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.read_sender().await?;
    let paths = [AttrPath::from_gp(&GenericPath::new(None, None, None))];
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    let mut chunks = 0;
    let mut attributes = 0;
    loop {
        chunks += 1;
        if let Some(reports) = &chunk.response()?.attr_reports {
            attributes += reports.iter().filter(|report| report.is_ok()).count();
        }
        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok((chunks, attributes)),
        }
    }
}

async fn subscribe_reachable(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [EventPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(57),
        Some(3),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .event_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

async fn set_node_label(client: &Matter<'_>, endpoint: u16, label: &str) -> Result<(), Error> {
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.write_sender(None).await?;
    let handle = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                let entries = builder.write_requests()?;
                let entry = entries
                    .push()?
                    .path(endpoint, 57, 5)?
                    .data(|writer| label.to_tlv(&TLVTag::Context(AttrDataTag::Data as u8), writer))?
                    .end()?;
                sender = entry.end()?.end()?;
            }
            TxOutcome::GotResponse(handle) => break handle,
        }
    };
    for status in handle.response()?.write_responses.iter() {
        assert_eq!(status?.status.status, IMStatusCode::Success);
    }
    Ok(())
}

async fn subscribe_identify(client: &Matter<'_>, endpoint: u16) -> Result<u32, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(endpoint),
        Some(3),
        Some(0),
    ))];
    let exchange = Exchange::initiate(
        client,
        test_only_crypto(),
        NonZeroU8::new(1).unwrap(),
        123456,
    )
    .await?;
    let mut sender = exchange.subscribe_sender().await?;
    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(false)?
                    .min_int_floor(0)?
                    .max_int_ceil(60)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(chunk) => break chunk,
        }
    };
    loop {
        let values = chunk
            .response()?
            .attrs::<u16>(3, 0)
            .map(|(_, value)| value.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, [1]);
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(subscription) => return Ok(subscription.subscription_id),
        }
    }
}

#[test]
fn real_im_temperature_subscription_reports_changes_once_and_ignores_same_value() {
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let service = DeviceService::new();
        let id = feature(FeatureRole::TemperatureSensor);
        let allocation = store.devices().allocate_feature(&id).unwrap();
        let endpoint = allocation.endpoint;
        let model = DeviceBridgeModel::new(service.clone(), store.devices(), store.matter()).unwrap();
        service.publish(id.clone(), "Temperature", FeatureCapabilities(vec![
            Capability::Temperature(NumericRange {
                minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
            }),
            Capability::Battery(NumericRange {
                minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
            }),
        ]));
        for index in 0..24 {
            let extra = FeatureIdentity {
                physical: PhysicalDeviceId {
                    account: AccountId::new("u").unwrap(),
                    home: HomeId::new("h").unwrap(),
                    parent_did: DeviceDid::new(format!("extra-{index}")).unwrap(),
                },
                service_instance: 2,
                role: FeatureRole::TemperatureSensor,
            };
            store.devices().allocate_feature(&extra).unwrap();
            service.publish(extra, format!("Extra {index}"), FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0,
                    maximum: 60.0,
                    step: 0.1,
                    unit: NumericUnit::Celsius,
                }),
            ]));
        }
        let basic_info = super::super::basic_info(&identity);
        let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let crypto = test_only_crypto();
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState = EthInteractionModelState::new(EthNetwork::new_default());
        let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
        super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
        connect(&server, 123456, 445566, 71);
        connect(&client, 445566, 123456, 71);
        let mut random = rand::rng();
        let handler = endpoints::EthSysHandlerBuilder::new().netif_diag(&SysNetifs).build(&mut random)
            .chain(|endpoint, _| endpoint != 0, &model);
        let im = InteractionModel::new(&server, &crypto, &buffers, (&model, &handler), &kv, &state);
        let incoming = Pipe::default();
        let outgoing = Pipe::default();
        let responder = DefaultResponder::new(&im);
        im.startup().await.unwrap();
        let services = async {
            or(server.run(&crypto, SendPipe(&outgoing), ReceivePipe(&incoming), NoNetwork),
                or(client.run(&crypto, SendPipe(&incoming), ReceivePipe(&outgoing), NoNetwork),
                    or(responder.run::<4, 4>(), im.run()))).await.unwrap();
            panic!("Matter service loop stopped before the controller completed");
        };
        let controller = async {
            async_io::Timer::after(Duration::from_millis(10)).await;
            while model.endpoint_for(&id).is_none() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 1);
            let (chunks, attributes) = read_wildcard_chunks(&client).await.unwrap();
            assert!(chunks > 1, "many endpoint reports must be chunked");
            assert!(attributes > 100, "all endpoint attributes must be reported");
            let subscription = subscribe_temperature(&client, endpoint).await.unwrap();
            while !state.subscriptions().has_subscription_for(NonZeroU8::new(1).unwrap(), 445566) {
                futures_lite::future::yield_now().await;
            }
            let values = BTreeMap::from([
                (Property::Temperature, PropertyValue::Temperature(21.25)),
                (Property::Battery, PropertyValue::Percent(Percent::new(75.0).unwrap())),
            ]);
            service.apply_report(StateReport::new(id.clone(), 1, StateSource::Lan, 1, values.clone()));
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                assert_eq!(rx.meta().proto_opcode, OpCode::ReportData as u8);
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(subscription));
                assert_eq!(
                    report.attrs::<Nullable<i16>>(temperature_measurement::FULL_CLUSTER.id, temperature_measurement::AttributeId::MeasuredValue as _)
                        .map(|(_, value)| value.unwrap().into_option())
                        .collect::<Vec<_>>(),
                    [Some(2125)]
                );
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            let battery = Context::new_at(&im, endpoint, 0x002f, 12).read_tlv(&model).await;
            let battery = TLVElement::new(&battery).structure().unwrap().find_ctx(1).unwrap()
                .structure().unwrap().find_ctx(2).unwrap();
            assert_eq!(Nullable::<u8>::from_tlv(&battery).unwrap().into_option(), Some(150));
            let status = Context::new_at(&im, endpoint, 0x002f, 0).read_tlv(&model).await;
            assert_eq!(
                power_source::PowerSourceStatusEnum::from_tlv(&value_element(&status)).unwrap(),
                power_source::PowerSourceStatusEnum::Unspecified
            );
            service.apply_report(StateReport::new(id.clone(), 2, StateSource::Lan, 2, values));
            let duplicate = or(
                async { Exchange::accept(&client).await.map(|_| true) },
                async { async_io::Timer::after(Duration::from_millis(100)).await; Ok(false) },
            ).await.unwrap();
            assert!(!duplicate, "same exposed value produced a second Matter report");
            for index in 0..300 {
                let temperature = if index == 299 { 22.0 } else if index % 2 == 0 { 20.0 } else { 21.0 };
                service.apply_report(StateReport::new(
                    id.clone(), service.next_report_version(), StateSource::Lan, index,
                    [(Property::Temperature, PropertyValue::Temperature(temperature))],
                ));
            }
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(subscription));
                assert_eq!(
                    report.attrs::<Nullable<i16>>(
                        temperature_measurement::FULL_CLUSTER.id,
                        temperature_measurement::AttributeId::MeasuredValue as _,
                    ).map(|(_, value)| value.unwrap().into_option()).collect::<Vec<_>>(),
                    [Some(2200)]
                );
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            assert!(!model.take_rebuild_request());
            assert_eq!(configuration_version(&store.matter()), 1);
            let reachable_subscription = subscribe_reachable(&client, endpoint).await.unwrap();
            service.set_state_availability(&id, false);
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(reachable_subscription));
                let events = report.event_reports.as_ref().unwrap().iter().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(events.len(), 1);
                let rs_matter::im::EventResp::Data(event) = &events[0] else { panic!("reachable report was not event data") };
                assert_eq!(event.path.to_gp(), GenericPath::new(Some(endpoint), Some(57), Some(3)));
                let event = rs_matter::dm::clusters::decl::bridged_device_basic_information::ReachableChanged::from_tlv(&event.data).unwrap();
                assert!(!event.reachable_new_value().unwrap());
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            service.set_state_availability(&id, true);
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(reachable_subscription));
                let events = report.event_reports.as_ref().unwrap().iter().collect::<Result<Vec<_>, _>>().unwrap();
                let rs_matter::im::EventResp::Data(event) = &events[0] else { panic!("reachable report was not event data") };
                let event = rs_matter::dm::clusters::decl::bridged_device_basic_information::ReachableChanged::from_tlv(&event.data).unwrap();
                assert!(event.reachable_new_value().unwrap());
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            invoke_identify(&client, endpoint, 1).await.unwrap();
            let identify_subscription = subscribe_identify(&client, endpoint).await.unwrap();
            let mut exchange = Exchange::accept(&client).await.unwrap();
            exchange.recv_fetch().await.unwrap();
            {
                let rx = exchange.rx().unwrap();
                let report = ReportDataResp::from_tlv(&TLVElement::new(rx.payload())).unwrap();
                assert_eq!(report.subscription_id, Some(identify_subscription));
                assert_eq!(report.attrs::<u16>(3, 0).map(|(_, value)| value.unwrap()).collect::<Vec<_>>(), [0]);
            }
            exchange.send_with(|_, buffer| {
                StatusResp::write(buffer, IMStatusCode::Success)?;
                Ok(Some(OpCode::StatusResponse.into()))
            }).await.unwrap();
            exchange.acknowledge().await.unwrap();
            drop(exchange);
            service.publish(id.clone(), "Catalog default", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
                Capability::Battery(NumericRange {
                    minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
                }),
            ]));
            futures_lite::future::yield_now().await;
            let label = Context::new_at(&im, endpoint, 57, 5).read_tlv(&model).await;
            assert_eq!(Utf8Str::from_tlv(&value_element(&label)).unwrap(), "Catalog default");
            set_node_label(&client, endpoint, "Controller label").await.unwrap();
            assert_eq!(
                store.matter().feature_label(endpoint).unwrap().as_deref(),
                Some("Controller label")
            );
            set_node_label(&client, endpoint, "").await.unwrap();
            assert_eq!(store.matter().feature_label(endpoint).unwrap().as_deref(), Some(""));
            service.publish(id.clone(), "Renamed by catalog", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
                Capability::Battery(NumericRange {
                    minimum: 0.0, maximum: 100.0, step: 1.0, unit: NumericUnit::Percent,
                }),
            ]));
            futures_lite::future::yield_now().await;
            assert_eq!(
                store.matter().feature_label(endpoint).unwrap().as_deref(),
                Some("")
            );
            let label = Context::new_at(&im, endpoint, 57, 5).read_tlv(&model).await;
            assert_eq!(Utf8Str::from_tlv(&value_element(&label)).unwrap(), "");
            service.publish(id.clone(), "Unsupported", FeatureCapabilities(vec![
                Capability::Power { writable: false },
            ]));
            while model.endpoint_for(&id).is_some() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 2);
            service.publish(id.clone(), "Restored sensor", FeatureCapabilities(vec![
                Capability::Temperature(NumericRange {
                    minimum: -20.0, maximum: 60.0, step: 0.05, unit: NumericUnit::Celsius,
                }),
            ]));
            while model.endpoint_for(&id).is_none() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 3);
            service.remove(&id);
            while model.endpoint_for(&id).is_some() {
                futures_lite::future::yield_now().await;
            }
            assert_eq!(configuration_version(&store.matter()), 4);
        };
        or(services, or(controller, async {
            async_io::Timer::after(Duration::from_secs(5)).await;
            panic!("timed out waiting for dynamic sensor subscription");
        })).await;
    })).unwrap().join().unwrap();
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
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let crypto = test_only_crypto();
                let buffers: MatterBuffers = MatterBuffers::new();
                let state: EthInteractionModelState =
                    EthInteractionModelState::new(EthNetwork::new_default());
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
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
                let basic_info = super::super::basic_info(&identity);
                let server = Matter::new(&basic_info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let client = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
                let kv = server.kv(super::super::storage::StoreAdapter::new(store.matter()));
                super::super::model::initialize_basic_info(&server, &kv, true).unwrap();
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
