//! A native device description validated once at the source boundary.

use super::{AppliedPatch, CapabilityError, ParsedDevice, description::Urn, patch};
use crate::device::PhysicalDeviceDescription;
use crate::xiaomi::legacy_miio::Mapping;

/// A derived transport adaptation; its diagnostic patch is not persisted.
#[derive(Clone, Debug)]
pub(crate) struct LegacyProfile {
    patch: patch::BuiltinPatch,
    target: String,
    mapping: &'static Mapping,
}

impl LegacyProfile {
    pub(super) fn new(
        patch: patch::BuiltinPatch,
        target: impl Into<String>,
        mapping: &'static Mapping,
    ) -> Self {
        Self {
            patch,
            target: target.into(),
            mapping,
        }
    }

    pub(crate) fn patch(&self) -> AppliedPatch {
        self.patch.applied(&self.target)
    }

    pub(in crate::xiaomi) fn target(&self) -> &str {
        &self.target
    }

    pub(in crate::xiaomi) fn mapping(&self) -> &'static Mapping {
        self.mapping
    }
}

impl PartialEq for LegacyProfile {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.mapping == other.mapping
    }
}
impl Eq for LegacyProfile {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDescription {
    model: String,
    instance_type: String,
    device: ParsedDevice,
    legacy_profile: Option<LegacyProfile>,
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
        let legacy_profile = patch::mcn02::profile(&model, &device);
        Ok(Self {
            model,
            instance_type,
            device,
            legacy_profile,
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

    pub(crate) fn legacy_profile(&self) -> Option<&LegacyProfile> {
        self.legacy_profile.as_ref()
    }

    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        self.device.physical_description(label)
    }
}
