pub(super) use crate::terminal::bridge::tests::support::terminal_command;

impl Bridge<'_> {
    /// Test convenience over the real advertisement lifecycle.
    pub(super) async fn run(
        &self,
        port: u16,
        report_pairing: impl Fn(PairingEvent) -> std::io::Result<()>,
    ) -> Result<(), RuntimeError> {
        self.run_with_advertisement(&mut crate::matter::Advertisement::new(port), report_pairing)
            .await
    }
}

pub(super) async fn next_power(
    receiver: &mut crate::device::CommandReceiver,
) -> Option<crate::device::CommandRequest> {
    match receiver.next_operation().await? {
        crate::device::DeviceRequest::Power(request) => Some(request),
        _ => panic!("unexpected lighting request"),
    }
}

mod context;
mod reports;
mod source;
pub(in crate::matter) use context::Context;
pub(super) use context::attr;
pub(super) use reports::{read_bytes, report_dataver, report_value};
pub(crate) use source::PublishedLightModel;

use super::*;
use crate::{
    device::CommandReceiver,
    matter::{FrozenEntry, LabelWriter, PublishedFunction, PublishedIdentity},
    storage::{StorageError, Store},
};
use std::rc::Rc;

pub(super) fn physical(
    id: &str,
    multi: bool,
) -> (crate::device::PhysicalDevice, Vec<CommandReceiver>) {
    let functions = if multi {
        vec![
            ("outlet_1", "开关 1"),
            ("outlet_2", "开关 2"),
            ("usb", "USB"),
        ]
    } else {
        vec![("main", "主电源")]
    };
    crate::device::PhysicalDevice::new(
        crate::device::PhysicalDeviceDescription::new(
            DeviceKey::new("xiaomi", id),
            format!("Outlet {id}"),
            functions
                .into_iter()
                .map(|(id, label)| crate::device::FunctionDescription {
                    id: id.into(),
                    label: label.into(),
                    kind: FunctionKind::Outlet,
                })
                .collect(),
        )
        .unwrap(),
    )
}
pub(crate) fn test_device() -> (PhysicalDevice, CommandReceiver) {
    let (device, mut receivers) = physical("test-outlet", false);
    device.set_label("Test Outlet".into());
    (device, receivers.remove(0))
}

pub(in crate::matter) fn light(rgb: bool) -> (PhysicalDevice, CommandReceiver) {
    let interval = crate::device::TemperatureInterval::new(2700, 6500).unwrap();
    light_with(if rgb {
        LightProfile::ExtendedColor(interval)
    } else {
        LightProfile::ColorTemperature(interval)
    })
}

pub(in crate::matter) fn light_with(profile: LightProfile) -> (PhysicalDevice, CommandReceiver) {
    let outlet = physical("test-light", false).0.description();
    let mut functions = outlet.functions().to_vec();
    functions[0].kind = FunctionKind::Light(profile);
    let description = crate::device::PhysicalDeviceDescription::new(
        outlet.key().clone(),
        outlet.label(),
        functions,
    )
    .unwrap();
    let (device, mut receivers) = PhysicalDevice::new(description);
    (device, receivers.remove(0))
}

/// Explicit protocol input; endpoints and function identities belong to the test.
pub(in crate::matter) fn entry(
    device: &PhysicalDevice,
    endpoint: u16,
    functions: &[(&str, u16)],
) -> FrozenEntry {
    FrozenEntry {
        device: device.clone(),
        identity: PublishedIdentity {
            endpoint,
            unique_id: format!("{endpoint:032x}"),
            configuration_version: 1,
            functions: functions
                .iter()
                .map(|(id, endpoint)| PublishedFunction {
                    id: (*id).into(),
                    endpoint: *endpoint,
                })
                .collect(),
        },
    }
}

pub(in crate::matter) fn single_entry(device: &PhysicalDevice) -> FrozenEntry {
    entry(device, 2, &[("main", 2)])
}

pub(in crate::matter) fn model_handler(entries: Vec<FrozenEntry>) -> crate::matter::model::Model {
    crate::matter::model::handler_from(entries, rand::rng(), Rc::new(NoLabels))
}

/// Label persistence tests supply the real App instead.
pub(super) struct NoLabels;
impl LabelWriter for NoLabels {
    fn write_label(&self, _: &DeviceKey, _: &str) -> Result<(), StorageError> {
        panic!("label writes require an application fixture")
    }
}

pub(super) fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = test_device();
    let bridge = Bridge::new(
        vec![single_entry(&device)],
        1,
        store,
        &identity,
        Rc::new(NoLabels),
    );
    block_on(or(bridge.run(0, |_| Ok(())), async {
        async_io::Timer::after(std::time::Duration::from_secs(5)).await;
        panic!("startup did not report invalid storage");
    }))
    .unwrap_err()
}
