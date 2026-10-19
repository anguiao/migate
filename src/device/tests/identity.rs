//! Device fixtures and identity checks for module and integration scenarios.

use crate::device::{
    CommandError, DeviceKey, DeviceRegistry, Function, LightAction, LightTarget, PhysicalDevice,
    PhysicalDeviceDescription,
};
use std::sync::Arc;

impl PhysicalDevice {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl DeviceRegistry {
    pub(crate) fn get(&self, key: &DeviceKey) -> Option<PhysicalDevice> {
        self.list().into_iter().find(|device| device.key() == key)
    }
}

impl PhysicalDevice {
    pub(crate) fn description(&self) -> PhysicalDeviceDescription {
        PhysicalDeviceDescription::new(
            self.key().clone(),
            self.label(),
            self.functions()
                .iter()
                .map(|function| function.description().clone())
                .collect(),
        )
        .unwrap()
    }
}

impl Function {
    pub(crate) fn begin_light_action(
        &self,
        target: LightTarget,
        final_frame: bool,
    ) -> Result<LightAction, CommandError> {
        self.begin_light_action_with_unchanged_startup(target, final_frame, false)
    }
}
