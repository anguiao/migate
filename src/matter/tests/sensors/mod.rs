//! Matter numeric sensor publication and visible measurement reports.

use super::super::{measurement_mapping_issue, model::basic_info, storage::StoreAdapter, units};
use super::support::{
    Context, attr, entry, model_handler, read_bytes, report_dataver, report_value,
};
use crate::device::{
    Availability, DeviceKey, FunctionDescription, FunctionKind, MeasurementRange, PhysicalDevice,
    PhysicalDeviceDescription,
};
use futures_lite::future::{block_on, poll_once};
use rs_matter::{
    MATTER_PORT, Matter,
    crypto::default_crypto,
    dm::{
        AsyncHandler, Metadata,
        devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM},
        networks::eth::EthNetwork,
    },
    error::ErrorCode,
    im::{EthInteractionModelState, InteractionModel},
    tlv::TLVElement,
    transport::exchange::MatterBuffers,
};
use std::pin::pin;

fn battery_sensor(battery: bool) -> PhysicalDevice {
    battery_sensor_named("sensor", "Sensor", battery)
}

fn battery_sensor_named(id: &str, label: &str, battery: bool) -> PhysicalDevice {
    let description = PhysicalDeviceDescription::new(
        DeviceKey::new("xiaomi", id),
        label,
        vec![
            FunctionDescription {
                id: "temperature".into(),
                label: "Temperature".into(),
                kind: FunctionKind::Temperature(MeasurementRange::new(-30.0, 100.0).unwrap()),
            },
            FunctionDescription {
                id: "humidity".into(),
                label: "Humidity".into(),
                kind: FunctionKind::Humidity(MeasurementRange::new(0.0, 100.0).unwrap()),
            },
        ],
    )
    .unwrap();
    let description = if battery {
        description.with_battery()
    } else {
        description
    };
    let (device, receivers) = PhysicalDevice::new(description);
    assert!(receivers.is_empty());
    device
}

macro_rules! setup {
    ($device:ident, $handler:ident, $im:ident) => {
        setup!($device, $handler, $im, false);
    };
    ($device:ident, $handler:ident, $im:ident, $battery:expr) => {
        setup!(@device $device, $handler, $im, battery_sensor($battery));
    };
    (@device $device:ident, $handler:ident, $im:ident, $physical:expr) => {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(dir.path()).unwrap();
        let identity = store.load_identity().unwrap();
        let $device = $physical;
        let info = basic_info(&identity);
        let matter = Matter::new(&info, TEST_DEV_COMM, &TEST_DEV_ATT, MATTER_PORT);
        let buffers: MatterBuffers = MatterBuffers::new();
        let state: EthInteractionModelState =
            EthInteractionModelState::new(EthNetwork::new_default());
        let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
        let kv = matter.kv(StoreAdapter::new(store.matter()));
        let functions = $device.functions().iter().enumerate().map(|(index, function)| (function.function_key().id.as_str(), index as u16 + 3)).collect::<Vec<_>>();
        let $handler = model_handler(vec![entry(&$device, 2, &functions)]);
        let $im = InteractionModel::new(&matter, &crypto, &buffers, &$handler, &kv, &state);
    };
}

mod battery;
mod co2;
mod illuminance;
mod measurements;
mod topology;
