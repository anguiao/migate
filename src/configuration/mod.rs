//! Pure configuration values, derived group graph and planning.
//!
//! Source facts use the Xiaomi native model directly; scope holds the current
//! account and home while the group id is derived. Storage owns only the
//! private codec and transaction. Planning receives the Matter canonical shape
//! explicitly and never reads storage, the network, randomness or time.

use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKind, PhysicalDeviceDescription},
    xiaomi::capability::{NativeDescription, RepairPlan},
};
use std::collections::{BTreeMap, BTreeSet};

mod plan;

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
    pub members: BTreeSet<String>,
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
    pub groups: Vec<GroupRelation>,
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
    pub memberships: BTreeMap<String, BTreeSet<String>>,
    pub rules: BTreeMap<String, GroupRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceDevice {
    pub key: DeviceKey,
    pub native: NativeDescription,
    pub name: Option<String>,
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

pub(crate) struct PlanInput {
    pub current: CommittedConfiguration,
    pub current_derived: DerivedConfiguration,
    pub current_shapes: BTreeMap<DeviceKey, crate::matter::FixedShape>,
    /// Effective source facts with non-fresh revocations already filtered out,
    /// so every supplied Matter shape matches a planned candidate.
    pub facts: SourceFacts,
    pub facts_shapes: BTreeMap<DeviceKey, crate::matter::FixedShape>,
    pub revocations: Vec<Revocation>,
    pub new_identities: BTreeMap<DeviceKey, String>,
}

impl DerivedConfiguration {
    pub(crate) fn visible_functions(&self, key: &DeviceKey) -> Vec<&FunctionDescription> {
        self.served[key]
            .description
            .functions()
            .iter()
            .filter(|function| !self.memberships.contains_key(&key.id) || !function.kind.is_light())
            .collect()
    }
}

impl CommittedConfiguration {
    pub(crate) fn empty() -> Self {
        Self {
            scope: None,
            root_version: 1,
            devices: Vec::new(),
            groups: Vec::new(),
        }
    }

    pub(crate) fn device(&self, key: &DeviceKey) -> Option<&CommittedDevice> {
        self.devices.iter().find(|device| &device.key == key)
    }

    /// Derive served descriptions, the published set and the group graph once.
    pub(crate) fn derive(&self) -> DerivedConfiguration {
        let groups = relations_by_did(&self.groups);
        derive_picture(
            self.devices.iter().filter_map(|device| {
                device.native.as_ref().map(|native| DerivationDevice {
                    key: &device.key,
                    native,
                    label: &device.label,
                })
            }),
            &groups,
        )
    }

    /// Structural and boundary validation, called when a record crosses the
    /// storage boundary. Internally planned values rely on their constructors
    /// and allocation invariants instead of repeating this check.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.root_version == 0 {
            return Err("Root configuration version is invalid");
        }
        if let Some(scope) = &self.scope {
            validate_text(&scope.uid)?;
            validate_text(&scope.home_id)?;
        } else if self.devices.iter().any(|device| device.native.is_some())
            || !self.groups.is_empty()
        {
            return Err("Committed configuration has facts without a home scope");
        }
        let mut keys = BTreeSet::new();
        let mut unique_ids = BTreeSet::new();
        let mut endpoints: BTreeMap<u16, DeviceKey> = BTreeMap::new();
        for device in &self.devices {
            validate_text(&device.key.source)?;
            validate_text(&device.key.id)?;
            if !keys.insert(device.key.clone()) {
                return Err("Committed device identity is duplicated");
            }
            if !uuid::Uuid::parse_str(&device.unique_id)
                .is_ok_and(|uuid| uuid.simple().to_string() == device.unique_id)
                || !unique_ids.insert(device.unique_id.clone())
            {
                return Err("Committed device unique id is invalid or duplicated");
            }
            if !(2..=65534).contains(&device.endpoint) || device.configuration_version == 0 {
                return Err("Committed device identity is invalid");
            }
            if device.label.len() > 32 {
                return Err("Committed device label is invalid");
            }
            if device.functions.is_empty() {
                return Err("Committed device function set is empty");
            }
            let mut ids = BTreeSet::new();
            let mut own_endpoints = BTreeSet::new();
            for function in &device.functions {
                validate_text(&function.id)?;
                if !ids.insert(function.id.as_str())
                    || !(2..=65534).contains(&function.endpoint)
                    || !own_endpoints.insert(function.endpoint)
                {
                    return Err("Committed device function is invalid");
                }
                reserve_endpoint(&mut endpoints, function.endpoint, &device.key)?;
            }
            let single = device.functions.len() == 1
                && device.native.as_ref().map_or_else(
                    || own_endpoints.contains(&device.endpoint),
                    |native| {
                        !native
                            .device()
                            .functions()
                            .iter()
                            .any(|function| function.kind().is_read_only())
                    },
                );
            if own_endpoints.contains(&device.endpoint) != single {
                return Err("Committed device endpoint does not match its function topology");
            }
            reserve_endpoint(&mut endpoints, device.endpoint, &device.key)?;
            if let Some(native) = &device.native {
                if native.device().key() != &device.key {
                    return Err("Committed native facts belong to another device");
                }
                for function in native.device().functions() {
                    if !device
                        .functions
                        .iter()
                        .any(|entry| entry.id == function.id())
                    {
                        return Err("Committed native function has no reserved identity");
                    }
                }
            }
        }
        let mut groups = BTreeSet::new();
        for group in &self.groups {
            validate_text(&group.group_did)?;
            validate_text(&group.record_did)?;
            validate_text(&group.model)?;
            if let Some(instance_type) = &group.instance_type {
                validate_text(instance_type)?;
            }
            if let Some(error) = &group.error {
                validate_text(error)?;
            }
            if !groups.insert(group.group_did.as_str()) {
                return Err("Committed group relation is duplicated");
            }
            let mut members = BTreeSet::new();
            for member in &group.members {
                validate_text(member)?;
                if !members.insert(member.as_str()) {
                    return Err("Committed group relation member is duplicated");
                }
            }
        }
        Ok(())
    }
}

impl SourceFacts {
    /// Derive the candidate picture from the latest source facts.
    pub(crate) fn derive(&self) -> DerivedConfiguration {
        let groups = relations_by_did(&self.relations);
        let labels = self.devices.iter().map(device_label).collect::<Vec<_>>();
        derive_picture(
            self.devices
                .iter()
                .zip(&labels)
                .map(|(device, label)| DerivationDevice {
                    key: &device.key,
                    native: &device.native,
                    label,
                }),
            &groups,
        )
    }
}

fn relations_by_did(relations: &[GroupRelation]) -> BTreeMap<String, GroupRelation> {
    relations
        .iter()
        .map(|relation| (relation.group_did.clone(), relation.clone()))
        .collect()
}

fn reserve_endpoint(
    endpoints: &mut BTreeMap<u16, DeviceKey>,
    endpoint: u16,
    key: &DeviceKey,
) -> Result<(), &'static str> {
    match endpoints.get(&endpoint) {
        Some(owner) if owner != key => Err("Committed endpoint is duplicated"),
        Some(_) => Ok(()),
        None => {
            endpoints.insert(endpoint, key.clone());
            Ok(())
        }
    }
}

pub(crate) fn device_label(device: &SourceDevice) -> String {
    device
        .name
        .as_deref()
        .map(import_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| default_device_label(device.native.model(), &device.key.id))
}

struct DerivationDevice<'a> {
    key: &'a DeviceKey,
    native: &'a NativeDescription,
    label: &'a str,
}

/// The one concrete derivation rule for a configuration picture. Committed
/// configurations and source facts only project their own fields into it.
fn derive_picture<'a>(
    devices: impl Iterator<Item = DerivationDevice<'a>>,
    groups: &BTreeMap<String, GroupRelation>,
) -> DerivedConfiguration {
    let devices = devices.collect::<Vec<_>>();
    let natives = devices
        .iter()
        .map(|device| (device.key.id.clone(), device.native))
        .collect::<BTreeMap<_, _>>();
    let publishable = groups
        .values()
        .filter(|group| group.active && natives.contains_key(&group.group_did))
        .map(|group| group.group_did.clone())
        .collect::<BTreeSet<_>>();
    let hidden = groups
        .values()
        .filter(|group| publishable.contains(&group.group_did))
        .flat_map(|group| group.members.iter().cloned())
        .collect::<BTreeSet<_>>();
    let rules = groups
        .values()
        .filter(|group| publishable.contains(&group.group_did))
        .map(|group| {
            (
                group.group_did.clone(),
                GroupRule {
                    group_did: group.group_did.clone(),
                    is_physical: group.is_physical,
                    members: group.members.iter().cloned().collect(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let served = devices
        .iter()
        .map(|device| {
            (
                device.key.clone(),
                derive_served(
                    device.native,
                    device.label,
                    groups.get(&device.key.id),
                    &natives,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let published = devices
        .iter()
        .filter(|device| {
            if groups.contains_key(&device.key.id) {
                publishable.contains(&device.key.id)
            } else {
                !hidden.contains(&device.key.id)
                    || device
                        .native
                        .device()
                        .functions()
                        .iter()
                        .any(|function| function.kind().is_read_only())
            }
        })
        .map(|device| device.key.clone())
        .collect();
    let memberships = {
        let mut memberships = BTreeMap::new();
        for rule in rules.values() {
            for member in &rule.members {
                memberships
                    .entry(member.clone())
                    .or_insert_with(BTreeSet::new)
                    .insert(rule.group_did.clone());
            }
        }
        memberships
    };
    DerivedConfiguration {
        served,
        published,
        groups: groups.clone(),
        memberships,
        rules,
    }
}

/// Build one served logical description from canonical native facts and the
/// committed graph. This is the only RGB repair / interval derivation.
fn derive_served(
    native: &NativeDescription,
    label: &str,
    group: Option<&GroupRelation>,
    member_natives: &BTreeMap<String, &NativeDescription>,
) -> ServedDevice {
    let description = native.physical_description(label);
    let Some(group) = group else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: None,
        };
    };
    let native_interval = native.device().functions()[0]
        .lighting()
        .and_then(|lighting| lighting.temperature())
        .map(|temperature| temperature.interval());
    let Some(native_interval) = native_interval else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: Some("native group has no color temperature".into()),
        };
    };
    let Some(members) = group
        .members
        .iter()
        .map(|did| member_natives.get(did).copied())
        .collect::<Option<Vec<_>>>()
    else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: Some(
                "every effective member needs a description before RGB repair".into(),
            ),
        };
    };
    let logical = members
        .iter()
        .map(|member| {
            member.device().functions()[0]
                .lighting()
                .and_then(|lighting| lighting.temperature())
                .map(|temperature| temperature.interval())
        })
        .collect::<Option<Vec<_>>>()
        .map(|intervals| {
            crate::xiaomi::capability::effective_temperature_range(native_interval, &intervals)
        })
        .unwrap_or(native_interval);
    let function = &native.device().functions()[0];
    let native_profile = function
        .lighting()
        .expect("a color-temperature group function has light bindings")
        .profile();
    let (repair, repair_reason, profile) = match crate::xiaomi::capability::plan_rgb_repair(
        native.device(),
        members.iter().copied(),
    ) {
        Ok(Some(plan)) => (
            Some(plan),
            None,
            crate::xiaomi::capability::repaired_profile(logical),
        ),
        Ok(None) => (
            None,
            None,
            crate::xiaomi::capability::narrowed_profile(native_profile, logical),
        ),
        Err(reason) => (
            None,
            Some(reason),
            crate::xiaomi::capability::narrowed_profile(native_profile, logical),
        ),
    };
    ServedDevice {
        description: with_light_kind(description, crate::device::FunctionKind::Light(profile)),
        repair,
        repair_reason,
    }
}

fn with_light_kind(
    description: PhysicalDeviceDescription,
    kind: FunctionKind,
) -> PhysicalDeviceDescription {
    let (key, label, mut functions, battery) = description.into_parts();
    functions
        .first_mut()
        .expect("a native group description has one main function")
        .kind = kind;
    let description = PhysicalDeviceDescription::new(key, label, functions)
        .expect("a light replacement keeps the description valid");
    if battery {
        description.with_battery()
    } else {
        description
    }
}

/// True when two native descriptions use the same real control bindings.
/// Model and instance type are metadata: a URN-only change reuses the served
/// instance.
pub(crate) fn same_control_description(a: &NativeDescription, b: &NativeDescription) -> bool {
    a.device().refresh_trigger() == b.device().refresh_trigger()
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
                    && left.measurement() == right.measurement()
            })
}

fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err("Committed configuration metadata is invalid")
    } else {
        Ok(())
    }
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
