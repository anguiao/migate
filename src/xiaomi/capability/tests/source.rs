//! Source fixtures retain the exact input used to recognize their native model.

use crate::{
    configuration::CommittedConfiguration,
    device::DeviceKey,
    xiaomi::capability::{
        BatteryBinding, CapabilityError, NativeDescription, ParsedFunction, PropertyBinding,
        RawDescription, RecognitionMetadata, SourceDescription,
    },
};
use serde_json::Value;

#[derive(Clone, Debug)]
pub(crate) struct SourceFixture {
    pub(crate) raw: RawDescription,
    pub(crate) native: NativeDescription,
}

impl SourceFixture {
    pub(crate) fn new(id: &str, model: &str, instance: Value) -> Self {
        let raw = RawDescription::new(
            DeviceKey::new("xiaomi", id),
            model.into(),
            None,
            instance["type"].as_str().unwrap().into(),
            instance,
            None,
        )
        .unwrap();
        let description = raw.recognize(false).unwrap();
        Self {
            raw: description.raw,
            native: description.native.unwrap(),
        }
    }

    pub(crate) fn committed(configuration: &CommittedConfiguration, id: &str) -> Self {
        let key = DeviceKey::new("xiaomi", id);
        Self {
            raw: configuration
                .descriptions
                .iter()
                .find(|raw| raw.key == key)
                .unwrap()
                .clone(),
            native: configuration.device(&key).unwrap().native.clone().unwrap(),
        }
    }
}

impl std::ops::Deref for SourceFixture {
    type Target = NativeDescription;

    fn deref(&self) -> &Self::Target {
        &self.native
    }
}

impl From<SourceFixture> for SourceDescription {
    fn from(fixture: SourceFixture) -> Self {
        Self {
            raw: fixture.raw,
            native: Some(fixture.native),
        }
    }
}

impl crate::xiaomi::capability::ParsedDevice {
    pub(crate) fn new(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::new_with_battery(key, functions, refresh_trigger, None)
    }
    pub(crate) fn new_with_battery(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::from_parts(
            key,
            functions,
            refresh_trigger,
            battery,
            None,
            RecognitionMetadata {
                category: "test".into(),
                core: Vec::new(),
                patches: Vec::new(),
                diagnostics: Vec::new(),
            },
        )
    }
}
