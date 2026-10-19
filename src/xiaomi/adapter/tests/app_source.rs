//! Controlled source resources for application lifecycle tests.

use std::rc::Rc;

use super::super::{
    SourceHandle, XiaomiAdapter,
    state::{DeviceSlot, LogicalSink},
    work::DeviceWork,
};
use crate::{
    configuration::NativeDescription,
    device::{CommandReceiver, DeviceKey, PhysicalDevice, PhysicalDeviceDescription},
    storage::{Store, tests::fixtures},
    xiaomi::{
        adapter::publication::SourceEntry, capability::tests::model_fixture::native_for,
        cloud::CloudClient, discovery::Discovery,
    },
};

impl SourceEntry {
    /// Build a source view for terminal tests; group views come from the real
    /// source install path, never from this helper.
    pub(crate) fn for_test(native: NativeDescription, physical: PhysicalDevice) -> Self {
        Self { native, physical }
    }
}

pub(crate) struct AppSource {
    pub(crate) adapter: XiaomiAdapter,
    pub(crate) handle: SourceHandle,
    pub(crate) physical: Option<PhysicalDevice>,
    pub(crate) receivers: Vec<CommandReceiver>,
}

impl AppSource {
    /// Restore a saved configuration, creating the requested initial identity
    /// only for a new database. No discovery or external transport is run.
    pub(crate) fn new(store: &Store, description: Option<PhysicalDeviceDescription>) -> Self {
        let mut configuration = store.devices().load_configuration().unwrap();
        if configuration.scope.is_none() {
            configuration.scope = Some(fixtures::scope());
            if let Some(description) = description {
                let mut identity = fixtures::identity(&description, 2);
                identity.native = Some(native_for(&description));
                configuration.devices.push(identity);
            }
            fixtures::save(store, &configuration);
        }
        let (_, events) = flume::unbounded();
        let (discovery, _driver) = Discovery::with_events(events);
        let (adapter, _) =
            XiaomiAdapter::new(store.clone(), CloudClient::new().unwrap(), discovery).unwrap();
        let derived = configuration.derive();
        let revocations = store.devices().load_revocations().unwrap();
        let mut receivers = Vec::new();
        let mut physical = None;
        {
            let mut live = adapter.context.live.borrow_mut();
            live.scope = configuration.scope;
            for device in configuration.devices {
                if revocations
                    .iter()
                    .any(|revocation| revocation.key == device.key)
                {
                    continue;
                }
                let Some(native) = device.native else {
                    continue;
                };
                let (instance, inputs) =
                    PhysicalDevice::new(derived.served[&device.key].description.clone());
                receivers.extend(inputs);
                adapter.context.registry.insert(instance.clone());
                let sink = Rc::new(LogicalSink::new(instance.clone()));
                let light_write = native
                    .device
                    .functions()
                    .iter()
                    .any(|function| function.lighting().is_some())
                    .then(|| {
                        adapter
                            .context
                            .light_writes
                            .for_device(&device.key.id, adapter.context.clock.clone())
                    });
                let work = Rc::new(DeviceWork::new(
                    native,
                    sink,
                    adapter.context.refresh_changed.clone(),
                    light_write,
                ));
                live.devices
                    .insert(device.key.id, DeviceSlot::restored(work));
                physical = Some(instance);
            }
        }
        let handle = adapter.source_handle();
        Self {
            adapter,
            handle,
            physical,
            receivers,
        }
    }

    pub(crate) fn directory_name(&self, key: &DeviceKey, name: &str) {
        let mut live = self.handle.0.live.borrow_mut();
        let slot = live.devices.get_mut(&key.id).unwrap();
        let native = slot.admitted.as_ref().unwrap().native.clone();
        slot.latest = Some(native);
        slot.latest_name = Some(name.to_owned());
    }
}
