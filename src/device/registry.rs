use super::{DeviceKey, PhysicalDevice};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct DeviceRegistry(Arc<Mutex<Vec<PhysicalDevice>>>);

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn list(&self) -> Vec<PhysicalDevice> {
        self.0.lock().unwrap().clone()
    }
    pub fn insert(&self, device: PhysicalDevice) {
        let mut devices = self.0.lock().unwrap();
        if devices.iter().any(|present| present.key() == device.key()) {
            return;
        }
        devices.push(device);
    }
    pub fn remove(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        let mut devices = self.0.lock().unwrap();
        let index = devices.iter().position(|device| device.key() == key)?;
        let removed = devices.remove(index);
        removed.close();
        Some(removed)
    }
}
