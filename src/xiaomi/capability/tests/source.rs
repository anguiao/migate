//! Source fixtures retain the exact input used to recognize their native model.

use crate::{
    configuration::CommittedConfiguration,
    device::DeviceKey,
    xiaomi::capability::{NativeDescription, RawDescription},
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
            instance["type"].as_str().unwrap().into(),
            instance,
            None,
        )
        .unwrap();
        let (raw, native) = raw.recognize(false).unwrap();
        Self {
            raw,
            native: native.unwrap(),
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
