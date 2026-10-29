//! Storage-private configuration encoding.

use crate::{
    configuration::{
        CommittedConfiguration, CommittedDevice, CommittedFunction, GroupRelation, Scope,
    },
    device::DeviceKey,
    xiaomi::capability::RawDescription,
};
use serde::{Deserialize, Deserializer, Serialize};
mod native;
use native::WireNative;

pub(super) fn decode(json: &str) -> Result<CommittedConfiguration, &'static str> {
    let wire: WireConfiguration =
        serde_json::from_str(json).map_err(|_| "Committed configuration is corrupted")?;
    wire.into_configuration()
        .map_err(|_| "Committed configuration is corrupted")
}

pub(super) fn encode(configuration: &CommittedConfiguration) -> Result<String, &'static str> {
    let wire = WireConfiguration::from_configuration(configuration);
    serde_json::to_string(&wire).map_err(|_| "Committed configuration cannot be encoded")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConfiguration {
    #[serde(default, deserialize_with = "present_option")]
    scope: Option<Option<WireScope>>,
    root_version: u32,
    devices: Vec<WireIdentity>,
    descriptions: Vec<WireDescription>,
    groups: Vec<WireGroup>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDescription {
    source: String,
    device_id: String,
    model: String,
    instance_type: String,
    instance: serde_json::Value,
    #[serde(default, deserialize_with = "present_option")]
    diagnostic: Option<Option<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireScope {
    uid: String,
    home_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIdentity {
    source: String,
    device_id: String,
    unique_id: String,
    endpoint: u16,
    label: String,
    configuration_version: u32,
    functions: Vec<WireFunctionIdentity>,
    #[serde(default, deserialize_with = "present_option")]
    native: Option<Option<WireNative>>,
}

/// Distinguish a missing critical field from an explicit null: missing stays
/// `None`, an explicit null becomes `Some(None)`.
fn present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunctionIdentity {
    id: String,
    endpoint: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireGroup {
    group_did: String,
    record_did: String,
    model: String,
    instance_type: Option<String>,
    is_physical: bool,
    active: bool,
    error: Option<String>,
    members: Vec<String>,
}

impl WireConfiguration {
    fn from_configuration(configuration: &CommittedConfiguration) -> Self {
        Self {
            scope: Some(configuration.scope.as_ref().map(|scope| WireScope {
                uid: scope.uid.clone(),
                home_id: scope.home_id.clone(),
            })),
            root_version: configuration.root_version,
            devices: configuration
                .devices
                .iter()
                .map(WireIdentity::from_committed)
                .collect(),
            descriptions: configuration
                .descriptions
                .iter()
                .map(WireDescription::from_raw)
                .collect(),
            groups: configuration
                .groups
                .iter()
                .map(WireGroup::from_relation)
                .collect(),
        }
    }

    fn into_configuration(self) -> Result<CommittedConfiguration, &'static str> {
        let scope = self
            .scope
            .ok_or("Stored home scope field is missing")?
            .map(|scope| Scope {
                uid: scope.uid,
                home_id: scope.home_id,
            });
        let configuration = CommittedConfiguration {
            scope,
            root_version: self.root_version,
            devices: self
                .devices
                .into_iter()
                .map(WireIdentity::into_committed)
                .collect::<Result<Vec<_>, _>>()?,
            descriptions: self
                .descriptions
                .into_iter()
                .map(WireDescription::into_raw)
                .collect::<Result<Vec<_>, _>>()?,
            groups: self
                .groups
                .into_iter()
                .map(WireGroup::into_relation)
                .collect(),
        };
        configuration.validate()?;
        Ok(configuration)
    }
}

impl WireDescription {
    fn from_raw(raw: &RawDescription) -> Self {
        Self {
            source: raw.key.source.clone(),
            device_id: raw.key.id.clone(),
            model: raw.model.clone(),
            instance_type: raw.instance_type.clone(),
            instance: raw.instance.clone(),
            diagnostic: Some(raw.diagnostic.clone()),
        }
    }

    fn into_raw(self) -> Result<RawDescription, &'static str> {
        RawDescription::new(
            DeviceKey::new(self.source, self.device_id),
            self.model,
            self.instance_type,
            self.instance,
            self.diagnostic
                .ok_or("Stored recognition diagnostic is missing")?,
        )
        .map_err(|_| "Stored source description is invalid")
    }
}

impl WireIdentity {
    fn from_committed(device: &CommittedDevice) -> Self {
        Self {
            source: device.key.source.clone(),
            device_id: device.key.id.clone(),
            unique_id: device.unique_id.clone(),
            endpoint: device.endpoint,
            label: device.label.clone(),
            configuration_version: device.configuration_version,
            functions: device
                .functions
                .iter()
                .map(|function| WireFunctionIdentity {
                    id: function.id.clone(),
                    endpoint: function.endpoint,
                })
                .collect(),
            native: Some(device.native.as_ref().map(WireNative::from_native)),
        }
    }

    fn into_committed(self) -> Result<CommittedDevice, &'static str> {
        let key = DeviceKey::new(self.source, self.device_id);
        let native = self
            .native
            .ok_or("Stored native facts field is missing")?
            .map(|native| native.into_native(&key))
            .transpose()?;
        Ok(CommittedDevice {
            key,
            unique_id: self.unique_id,
            endpoint: self.endpoint,
            label: self.label,
            configuration_version: self.configuration_version,
            functions: self
                .functions
                .into_iter()
                .map(|function| CommittedFunction {
                    id: function.id,
                    endpoint: function.endpoint,
                })
                .collect(),
            native,
        })
    }
}

impl WireGroup {
    fn from_relation(relation: &GroupRelation) -> Self {
        Self {
            group_did: relation.group_did.clone(),
            record_did: relation.record_did.clone(),
            model: relation.model.clone(),
            instance_type: relation.instance_type.clone(),
            is_physical: relation.is_physical,
            active: relation.active,
            error: relation.error.clone(),
            members: relation.members.clone(),
        }
    }

    fn into_relation(self) -> GroupRelation {
        GroupRelation {
            group_did: self.group_did,
            record_did: self.record_did,
            model: self.model,
            instance_type: self.instance_type,
            is_physical: self.is_physical,
            active: self.active,
            error: self.error,
            members: self.members,
        }
    }
}
