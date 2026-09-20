mod climate;
mod curtain;
mod fan;
mod interaction;
mod lighting;
mod sensing;
mod subscription_recovery;
mod topology;
mod vacuum;

use super::handlers::Context;
use super::subscriptions::{Pipe, ReceivePipe, SendPipe, connect, connect_at};
use crate::matter::{
    DeviceBridgeModel,
    lighting::{LightingHandler, SceneOnOff, rgb_to_xy},
    sensors,
    topology::config_signature,
};
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
use interaction::{
    expect_temperature_report, group_membership, invoke_command, invoke_identify,
    read_wildcard_chunks, scene_info, set_node_label, subscribe_curtain_position,
    subscribe_fan_percent, subscribe_identify, subscribe_level, subscribe_reachable,
    subscribe_rvc_fault, subscribe_temperature, subscribe_thermostat_cooling,
};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::test_only_crypto,
    dm::{
        AsyncHandler, InvokeContext, InvokeReplyInstance, Metadata,
        clusters::{
            app::color_control::{RgbGamma, SetDeviceColor},
            decl::{
                color_control, fan_control, globals, groups, level_control, occupancy_sensing,
                on_off, power_source, relative_humidity_measurement, rvc_clean_mode,
                rvc_operational_state, rvc_run_mode, scenes_management, temperature_measurement,
                thermostat, window_covering,
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
    path::Path,
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

#[derive(Default)]
struct CurtainCommands {
    calls: RefCell<Vec<(FeatureIdentity, Vec<DeviceCommand>)>>,
    stopped: RefCell<Vec<(FeatureIdentity, Property)>>,
}

impl DeviceCommandSink for CurtainCommands {
    fn submit(
        &self,
        feature: FeatureIdentity,
        commands: Vec<DeviceCommand>,
    ) -> futures_util::future::LocalBoxFuture<'static, CommandOutcome> {
        self.calls.borrow_mut().push((feature, commands));
        Box::pin(async { CommandOutcome::Accepted })
    }

    fn stop_adjustment(&self, feature: &FeatureIdentity, property: Property) {
        self.stopped.borrow_mut().push((feature.clone(), property));
    }
}

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

fn command_response_element(bytes: &[u8]) -> TLVElement<'_> {
    TLVElement::new(bytes)
        .structure()
        .unwrap()
        .find_ctx(0)
        .unwrap()
        .structure()
        .unwrap()
        .find_ctx(1)
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
