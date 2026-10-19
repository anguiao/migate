//! Test-only accessors for the private physical description fields.

use super::PhysicalDeviceDescription;

impl PhysicalDeviceDescription {
    pub(crate) fn key(&self) -> &crate::device::DeviceKey {
        &self.key
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}
