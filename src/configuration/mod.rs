//! Pure configuration values, derived group graph and planning.
//!
//! Source facts use the Xiaomi native model directly; scope holds the current
//! account and home while the group id is derived. Storage owns only the
//! private codec and transaction. Planning receives a pure publication shape
//! projection and never reads storage, the network, randomness or time.

use crate::{
    device::{DeviceKey, FunctionKey, PhysicalDeviceDescription},
    xiaomi::capability::{NativeDescription, RawDescription, RepairPlan, SourceDescription},
};
use std::collections::{BTreeMap, BTreeSet};

mod derivation;
mod plan;
mod validation;

pub(crate) use plan::plan;

/// The current account and home. The bridge group id is derived, never stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Scope {
    pub uid: String,
    pub home_id: String,
}
impl Scope {
    pub(crate) fn group_id(&self) -> String {
        crate::xiaomi::cloud::group_id(&self.uid, &self.home_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GroupRelation {
    pub group_did: String,
    pub record_did: String,
    pub model: String,
    pub instance_type: Option<String>,
    pub is_physical: bool,
    pub active: bool,
    pub error: Option<String>,
    pub members: Vec<String>,
}

/// The execution-relevant rule of one effective group. Directory metadata such
/// as the model, URN or a diagnostic error is deliberately excluded: only
/// these values decide hiding, feedback ownership and repair attribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GroupRule {
    pub group_did: String,
    pub is_physical: bool,
    pub members: BTreeSet<FunctionKey>,
    pub confirmed_members: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Revocation {
    pub key: DeviceKey,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedFunction {
    pub id: String,
    pub endpoint: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedDevice {
    pub key: DeviceKey,
    pub unique_id: String,
    pub endpoint: u16,
    pub label: String,
    pub configuration_version: u32,
    pub functions: Vec<CommittedFunction>,
    pub native: Option<NativeDescription>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedConfiguration {
    pub scope: Option<Scope>,
    pub root_version: u32,
    pub devices: Vec<CommittedDevice>,
    pub descriptions: Vec<RawDescription>,
    pub groups: Vec<GroupRelation>,
    pub temperature_bindings: BTreeMap<FunctionKey, FunctionKey>,
}

impl Default for CommittedConfiguration {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServedDevice {
    pub description: PhysicalDeviceDescription,
    pub repair: Option<RepairPlan>,
    pub repair_reason: Option<String>,
}

/// One derived picture of a configuration: served logical descriptions, the
/// published set, the forward/reverse group index and the execution rules.
/// Derived once per configuration and carried by the plan for atomic
/// installation; consumers never re-interpret the raw relations.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub(crate) struct DerivedConfiguration {
    pub served: BTreeMap<DeviceKey, ServedDevice>,
    pub published: BTreeSet<DeviceKey>,
    pub groups: BTreeMap<String, GroupRelation>,
    pub memberships: BTreeMap<FunctionKey, BTreeSet<String>>,
    pub member_issues: BTreeMap<String, Vec<String>>,
    pub rules: BTreeMap<String, GroupRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceDevice {
    pub description: SourceDescription,
    pub fresh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub(crate) struct SourceFacts {
    pub scope: Option<Scope>,
    pub devices: Vec<SourceDevice>,
    pub relations: Vec<GroupRelation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationKind {
    Added,
    Removed,
    Reused,
    /// The real control bindings changed; served instances are replaced.
    ReplacedNative,
    /// The same control description is kept but the previous authority was
    /// withdrawn (revocation re-admission or scope generation); the serving
    /// instance is replaced without retiring raw transport feedback.
    ReplacedAuthority,
    ReplacedLogical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlanOperation {
    pub key: DeviceKey,
    pub kind: OperationKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlanError {
    EndpointExhausted { key: DeviceKey },
    VersionExhausted { key: DeviceKey },
    MissingIdentity { key: DeviceKey },
    InvalidInput(&'static str),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointExhausted { key } => {
                write!(
                    formatter,
                    "Device endpoint numbers are exhausted for {}",
                    key.id
                )
            }
            Self::VersionExhausted { key } => write!(
                formatter,
                "Device configuration version is exhausted for {}",
                key.id
            ),
            Self::MissingIdentity { key } => write!(
                formatter,
                "New device identity was not supplied for {}",
                key.id
            ),
            Self::InvalidInput(reason) => formatter.write_str(reason),
        }
    }
}

/// The complete planning result. The derived graph belongs to the next
/// committed configuration and is installed atomically with it.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub configuration: CommittedConfiguration,
    pub derived: std::rc::Rc<DerivedConfiguration>,
    pub operations: Vec<PlanOperation>,
    pub clear_revocations: Vec<DeviceKey>,
    pub rebuild: bool,
    pub changed: bool,
}

pub(crate) struct PlanInput<'a> {
    pub current: &'a CommittedConfiguration,
    pub current_derived: &'a DerivedConfiguration,
    pub facts: SourceFacts,
    pub revocations: Vec<Revocation>,
    pub new_identities: BTreeMap<DeviceKey, String>,
}

impl CommittedConfiguration {
    /// Join the validated stored input with its effective model once per source.
    pub(crate) fn source_descriptions(&self) -> impl Iterator<Item = SourceDescription> + '_ {
        self.descriptions.iter().map(|raw| SourceDescription {
            raw: raw.clone(),
            native: self
                .device(&raw.key)
                .and_then(|device| device.native.clone()),
        })
    }

    pub(crate) fn empty() -> Self {
        Self {
            scope: None,
            root_version: 1,
            devices: Vec::new(),
            descriptions: Vec::new(),
            groups: Vec::new(),
            temperature_bindings: BTreeMap::new(),
        }
    }

    pub(crate) fn device(&self, key: &DeviceKey) -> Option<&CommittedDevice> {
        self.devices.iter().find(|device| &device.key == key)
    }
}

/// True when two native descriptions use the same bindings and source semantics.
/// Metadata-only changes reuse the served instance; a different derived direct
/// profile changes transport semantics and requires a new source runtime.
pub(crate) fn same_control_description(a: &NativeDescription, b: &NativeDescription) -> bool {
    a.legacy_profile() == b.legacy_profile()
        && a.device().refresh_trigger() == b.device().refresh_trigger()
        && a.device().battery() == b.device().battery()
        && a.device().functions().len() == b.device().functions().len()
        && a.device()
            .functions()
            .iter()
            .zip(b.device().functions())
            .all(|(left, right)| {
                left.id() == right.id()
                    && left.power() == right.power()
                    && left.lighting() == right.lighting()
                    && left.curtain() == right.curtain()
                    && left.fan() == right.fan()
                    && left.air_conditioner() == right.air_conditioner()
                    && left.measurement() == right.measurement()
                    && left.state() == right.state()
                    && left.motion() == right.motion()
            })
}

/// Import a name without splitting a UTF-8 character. Protocol writes use validation instead.
pub(crate) fn import_label(label: &str) -> String {
    let mut end = label.len().min(32);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].to_owned()
}

pub(crate) fn validate_label(label: &str) -> Result<(), &'static str> {
    if label.len() > 32 {
        Err("Device label exceeds 32 bytes")
    } else {
        Ok(())
    }
}

pub(crate) fn default_device_label(model: &str, id: &str) -> String {
    let mut start = id.len().saturating_sub(16);
    while !id.is_char_boundary(start) {
        start += 1;
    }
    let suffix = &id[start..];
    let mut end = model.len().min(32 - suffix.len() - 3);
    while !model.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} ({suffix})", &model[..end])
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
