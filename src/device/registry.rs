use super::{DeviceKey, PhysicalDevice};
use event_listener::Event;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct RegistrySnapshot {
    pub revision: u64,
    pub devices: Vec<PhysicalDevice>,
}
#[derive(Clone)]
pub struct DeviceRegistry(Arc<Inner>);
struct Inner {
    snapshot: Mutex<RegistrySnapshot>,
    changed: Event,
}
impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl DeviceRegistry {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            snapshot: Mutex::new(RegistrySnapshot {
                revision: 0,
                devices: Vec::new(),
            }),
            changed: Event::new(),
        }))
    }
    pub fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        self.0
            .snapshot
            .lock()
            .unwrap()
            .devices
            .iter()
            .find(|device| device.key() == key)
            .cloned()
    }
    pub fn list(&self) -> Vec<PhysicalDevice> {
        self.0.snapshot.lock().unwrap().devices.clone()
    }
    pub fn insert(&self, device: PhysicalDevice) {
        let mut state = self.0.snapshot.lock().unwrap();
        if state
            .devices
            .iter()
            .any(|present| present.key() == device.key())
        {
            return;
        }
        state.devices.push(device);
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
    }
    pub fn remove(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        let mut state = self.0.snapshot.lock().unwrap();
        let index = state
            .devices
            .iter()
            .position(|device| device.key() == key)?;
        let removed = state.devices.remove(index);
        removed.close();
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.notify(usize::MAX);
        Some(removed)
    }
    pub fn subscribe(&self) -> RegistryChanges {
        RegistryChanges {
            registry: self.clone(),
            revision: self.0.snapshot.lock().unwrap().revision,
        }
    }
}
pub struct RegistryChanges {
    registry: DeviceRegistry,
    revision: u64,
}
impl RegistryChanges {
    pub async fn changed(&mut self) -> RegistrySnapshot {
        loop {
            let listener = self.registry.0.changed.listen();
            let snapshot = self.registry.0.snapshot.lock().unwrap().clone();
            if snapshot.revision != self.revision {
                self.revision = snapshot.revision;
                return snapshot;
            }
            listener.await;
        }
    }
}
