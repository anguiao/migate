//! The unchanged source instance and its most recently committed recognition result.

use super::{CapabilityError, NativeDescription, description::Description, parse_device_for_model};
use crate::device::DeviceKey;
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawDescription {
    pub(crate) key: DeviceKey,
    pub(crate) model: String,
    pub(crate) instance_type: String,
    pub(crate) instance: Value,
    pub(crate) diagnostic: Option<String>,
}

impl RawDescription {
    /// Validate source identity and the complete document at the boundary.
    pub(crate) fn new(
        key: DeviceKey,
        model: String,
        instance_type: String,
        instance: Value,
        diagnostic: Option<String>,
    ) -> Result<Self, CapabilityError> {
        super::validate_source_identity(&key)?;
        if model.is_empty() || model.chars().any(char::is_control) {
            return Err(CapabilityError::InvalidDescription("native model"));
        }
        if instance.get("type").and_then(Value::as_str) != Some(instance_type.as_str()) {
            return Err(CapabilityError::InvalidDescription(
                "instance type mismatch",
            ));
        }
        Description::parse(&instance)?;
        if diagnostic
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(CapabilityError::InvalidDescription(
                "recognition diagnostic",
            ));
        }
        Ok(Self {
            key,
            model,
            instance_type,
            instance,
            diagnostic,
        })
    }

    pub(crate) fn is_group_instance(&self) -> bool {
        Description::parse(&self.instance)
            .expect("a source description has already been validated")
            .urn
            .standard("device", "light", 0x0000a001)
    }

    /// Re-run the current parser without changing the original source document.
    pub(crate) fn recognize(
        &self,
        group: bool,
    ) -> Result<(Self, Option<NativeDescription>), CapabilityError> {
        let parsed = if group {
            super::parse_group(self.key.clone(), &self.model, &self.instance)
        } else {
            parse_device_for_model(self.key.clone(), &self.model, &self.instance)
        };
        let mut source = self.clone();
        match parsed {
            Ok(device) => {
                source.diagnostic = None;
                let native =
                    NativeDescription::new(self.model.clone(), self.instance_type.clone(), device)?;
                Ok((source, Some(native)))
            }
            Err(
                error @ (CapabilityError::UnsupportedDevice | CapabilityError::NoCapabilities(_)),
            )
            | Err(error @ CapabilityError::InvalidGroup("group light is missing or ambiguous")) => {
                source.diagnostic = Some(error.to_string());
                Ok((source, None))
            }
            Err(error) => Err(error),
        }
    }
}
