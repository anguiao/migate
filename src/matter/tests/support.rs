pub(super) use crate::terminal::bridge::tests::support::terminal_command;
pub(super) use crate::xiaomi::capability::tests::model_fixture::native_for;

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
        crate::device::DeviceRequest::Light(_) => panic!("unexpected lighting request"),
    }
}

mod context;
mod reports;
pub(in crate::matter) use context::Context;
pub(super) use context::attr;
pub(super) use reports::{read_bytes, report_dataver, report_value};

use super::*;
use crate::{
    configuration::{CommittedConfiguration, CommittedDevice, CommittedFunction},
    device::CommandReceiver,
    matter::{FrozenEntry, LabelWriter, PublishedFunction, PublishedIdentity},
    storage::{StorageError, Store},
};
use std::rc::Rc;

fn next_endpoint(configuration: &CommittedConfiguration) -> u16 {
    configuration
        .devices
        .iter()
        .flat_map(|device| {
            std::iter::once(device.endpoint)
                .chain(device.functions.iter().map(|function| function.endpoint))
        })
        .max()
        .unwrap_or(1)
        + 1
}

pub(super) fn add_device(
    store: &Store,
    description: &crate::device::PhysicalDeviceDescription,
) -> CommittedDevice {
    let mut configuration = store.devices().load_configuration().unwrap();
    assert!(
        configuration
            .devices
            .iter()
            .all(|device| &device.key != description.key())
    );
    let endpoint = next_endpoint(&configuration);
    let single = description.functions().len() == 1;
    let functions = description
        .functions()
        .iter()
        .enumerate()
        .map(|(index, function)| CommittedFunction {
            id: function.id.clone(),
            endpoint: if single {
                endpoint
            } else {
                endpoint + 1 + index as u16
            },
        })
        .collect();
    let stored = CommittedDevice {
        key: description.key().clone(),
        unique_id: uuid::Uuid::new_v4().simple().to_string(),
        endpoint,
        label: crate::configuration::import_label(description.label()),
        configuration_version: 1,
        functions,
        native: Some(native_for(description)),
    };
    if configuration.scope.is_none() {
        configuration.scope = Some(crate::configuration::Scope {
            uid: "account".into(),
            home_id: "home".into(),
        });
    }
    configuration.devices.push(stored.clone());
    configuration.devices.sort_by(|a, b| a.key.cmp(&b.key));
    store
        .devices()
        .commit_configuration(&configuration, &[])
        .unwrap();
    stored
}

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
pub(crate) fn test_registry(
    store: &Store,
    device: &PhysicalDevice,
) -> crate::device::DeviceRegistry {
    let stored = store
        .devices()
        .load_configuration()
        .unwrap()
        .device(device.key())
        .cloned()
        .unwrap_or_else(|| add_device(store, &device.description()));
    device.set_label(stored.label);
    let registry = crate::device::DeviceRegistry::new();
    registry.insert(device.clone());
    registry
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

/// Build one frozen Matter handler from the test registry and its committed
/// identities. Test-only construction; production receives explicit entries.
pub(in crate::matter) fn model_handler(
    registry: &crate::device::DeviceRegistry,
    store: &Store,
    _protocol: crate::matter::storage::StoreAdapter,
    random: impl rand::Rng,
) -> Result<crate::matter::model::Model, StorageError> {
    let entries = frozen_entries(registry, store)?;
    crate::matter::model::handler_from(entries, store, random, renamer(registry, store))
}

pub(super) fn frozen_entries(
    registry: &crate::device::DeviceRegistry,
    store: &Store,
) -> Result<Vec<FrozenEntry>, StorageError> {
    let configuration = store.devices().load_configuration()?;
    let mut entries = Vec::new();
    for device in registry.list() {
        let committed = configuration.device(device.key()).ok_or_else(|| {
            StorageError::new(
                store.path(),
                "freeze Matter topology",
                "Test device has no committed identity",
            )
        })?;
        let functions = device
            .description()
            .functions()
            .iter()
            .map(|function| {
                let endpoint = committed
                    .functions
                    .iter()
                    .find(|entry| entry.id == function.id)
                    .map(|entry| entry.endpoint)
                    .ok_or_else(|| {
                        StorageError::new(
                            store.path(),
                            "freeze Matter topology",
                            "Test function has no reserved endpoint",
                        )
                    })?;
                Ok(PublishedFunction {
                    id: function.id.clone(),
                    endpoint,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        entries.push(FrozenEntry {
            device,
            identity: PublishedIdentity {
                endpoint: committed.endpoint,
                unique_id: committed.unique_id.clone(),
                configuration_version: committed.configuration_version,
                functions,
            },
        });
    }
    entries.sort_by_key(|entry| entry.identity.endpoint);
    Ok(entries)
}

/// The explicit publication identity of one committed test device.
pub(super) fn published_identity(store: &Store, key: &DeviceKey) -> PublishedIdentity {
    let configuration = store.devices().load_configuration().unwrap();
    let committed = configuration.device(key).unwrap();
    PublishedIdentity {
        endpoint: committed.endpoint,
        unique_id: committed.unique_id.clone(),
        configuration_version: committed.configuration_version,
        functions: committed
            .functions
            .iter()
            .map(|function| PublishedFunction {
                id: function.id.clone(),
                endpoint: function.endpoint,
            })
            .collect(),
    }
}

pub(super) fn renamer(
    registry: &crate::device::DeviceRegistry,
    store: &Store,
) -> Rc<dyn LabelWriter> {
    Rc::new(StoreRenamer {
        store: store.clone(),
        registry: registry.clone(),
    })
}

/// Test label writer: persists through the same private configuration record.
struct StoreRenamer {
    store: Store,
    registry: crate::device::DeviceRegistry,
}
impl LabelWriter for StoreRenamer {
    fn write_label(&self, key: &DeviceKey, label: &str) -> Result<(), StorageError> {
        let mut configuration = self.store.devices().load_configuration()?;
        let device = configuration
            .devices
            .iter_mut()
            .find(|device| &device.key == key)
            .ok_or_else(|| {
                StorageError::new(
                    self.store.path(),
                    "write device label",
                    "Device identity does not exist",
                )
            })?;
        device.label = label.to_owned();
        self.store
            .devices()
            .commit_configuration(&configuration, &[])?;
        self.registry
            .get(key)
            .expect("test renamer knows its registry devices")
            .set_label(label.to_owned());
        Ok(())
    }
}

pub(super) fn startup_error(store: &Store) -> RuntimeError {
    let identity = store.load_identity().unwrap();
    let (device, _receiver) = test_device();
    let registry = test_registry(store, &device);
    let entries = frozen_entries(&registry, store).unwrap();
    let renamer = renamer(&registry, store);
    let bridge = Bridge::new(entries, 1, store, &identity, renamer);
    block_on(or(bridge.run(0, |_| Ok(())), async {
        async_io::Timer::after(std::time::Duration::from_secs(5)).await;
        panic!("startup did not report invalid storage");
    }))
    .unwrap_err()
}
