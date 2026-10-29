//! A native device description validated once at the source boundary.

use super::{CapabilityError, ParsedDevice, description::Urn, patch};
use crate::device::PhysicalDeviceDescription;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDescription {
    model: String,
    instance_type: String,
    device: ParsedDevice,
}

impl NativeDescription {
    pub(crate) fn new(
        model: String,
        instance_type: String,
        device: ParsedDevice,
    ) -> Result<Self, CapabilityError> {
        if [&model, &instance_type]
            .iter()
            .any(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(CapabilityError::InvalidDescription(
                "native model or instance type",
            ));
        }
        let urn = Urn::parse(&instance_type)
            .map_err(|_| CapabilityError::InvalidDescription("native instance type"))?;
        if urn.entity != "device" {
            return Err(CapabilityError::InvalidDescription("native instance type"));
        }
        patch::validate_native(&device, &model, &urn)
            .map_err(CapabilityError::InvalidDescription)?;
        Ok(Self {
            model,
            instance_type,
            device,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    pub(crate) fn instance_type(&self) -> &str {
        &self.instance_type
    }
    pub(crate) fn device(&self) -> &ParsedDevice {
        &self.device
    }

    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        self.device.physical_description(label)
    }
}
