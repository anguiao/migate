//! Test-only identity comparison of serving physical devices.

use super::PhysicalDevice;
use std::sync::Arc;

impl PhysicalDevice {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}
