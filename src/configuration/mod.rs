//! Pure configuration values, derived group graph and planning.
//!
//! Source facts use the Xiaomi native model directly; scope holds the current
//! account and home while the group id is derived. Storage owns only the
//! private codec and transaction. Planning receives a pure publication shape
//! projection and never reads storage, the network, randomness or time.

use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKey, PhysicalDeviceDescription},
    xiaomi::capability::{NativeDescription, RawDescription, RepairPlan},
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
    pub key: DeviceKey,
    pub native: NativeDescription,
    pub name: Option<String>,
    pub fresh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub(crate) struct SourceFacts {
    pub scope: Option<Scope>,
    pub devices: Vec<SourceDevice>,
    pub descriptions: Vec<SourceRaw>,
    pub relations: Vec<GroupRelation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceRaw {
    pub description: RawDescription,
    pub fresh: bool,
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

impl DerivedConfiguration {
    pub(crate) fn visible_functions(&self, key: &DeviceKey) -> Vec<&FunctionDescription> {
        self.served[key]
            .description
            .functions()
            .iter()
            .filter(|function| {
                !self
                    .memberships
                    .contains_key(&FunctionKey::new(key.clone(), &function.id))
            })
            .collect()
    }
}

impl CommittedConfiguration {
    pub(crate) fn empty() -> Self {
        Self {
            scope: None,
            root_version: 1,
            devices: Vec::new(),
            descriptions: Vec::new(),
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
            || !self.descriptions.is_empty()
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
            if own_endpoints.contains(&device.endpoint) {
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
        let mut sources = BTreeSet::new();
        for source in &self.descriptions {
            if !sources.insert(source.key.clone()) {
                return Err("Committed source description is duplicated");
            }
            let effective = self
                .device(&source.key)
                .and_then(|device| device.native.as_ref());
            match (effective, &source.diagnostic) {
                (Some(native), None)
                    if native.model() == source.model
                        && native.instance_type() == source.instance_type => {}
                (None, Some(_)) => {}
                _ => return Err("Committed source and effective description disagree"),
            }
        }
        if self
            .devices
            .iter()
            .any(|device| device.native.is_some() && !sources.contains(&device.key))
        {
            return Err("Committed source description is missing");
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
            if let Some(source) = self
                .descriptions
                .iter()
                .find(|source| source.key.id == group.group_did)
            {
                if !source.is_group_instance() {
                    return Err("Committed group source is invalid");
                }
                if self
                    .device(&source.key)
                    .and_then(|device| device.native.as_ref())
                    .is_some_and(|native| native.device().unique_light().is_none())
                {
                    return Err("Committed group effective description is invalid");
                }
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

struct DerivationDevice<'a> {
    key: &'a DeviceKey,
    native: &'a NativeDescription,
    label: &'a str,
}

/// Resolved member identities and relationship diagnostics from the same
/// candidate configuration. Unresolved members never claim a function.
struct ResolvedMembers {
    members: Vec<FunctionKey>,
    issues: Vec<String>,
}

fn resolve_members(
    group: &GroupRelation,
    natives: &BTreeMap<String, &NativeDescription>,
) -> ResolvedMembers {
    let mut members = Vec::new();
    let mut issues = Vec::new();
    for did in &group.members {
        let Some(native) = natives.get(did) else {
            issues.push(format!("member {did} description is missing"));
            continue;
        };
        let lights = native
            .device()
            .functions()
            .iter()
            .filter(|function| function.kind().is_light())
            .collect::<Vec<_>>();
        match lights.as_slice() {
            [function] => members.push(FunctionKey::new(
                native.device().key().clone(),
                function.id(),
            )),
            [] => issues.push(format!("member {did} has no valid light function")),
            _ => issues.push(format!("member {did} light function is ambiguous")),
        }
    }
    ResolvedMembers { members, issues }
}

/// Derive the served graph from one complete configuration, using its saved
/// labels, native descriptions and canonical group relations together.
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
    let resolved = groups
        .values()
        .map(|group| (group.group_did.clone(), resolve_members(group, &natives)))
        .collect::<BTreeMap<_, _>>();
    let rules = groups
        .values()
        .filter(|group| publishable.contains(&group.group_did))
        .map(|group| {
            (
                group.group_did.clone(),
                GroupRule {
                    group_did: group.group_did.clone(),
                    is_physical: group.is_physical,
                    members: resolved[&group.group_did].members.iter().cloned().collect(),
                    confirmed_members: group.members.len(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
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
    let served = devices
        .iter()
        .map(|device| {
            (
                device.key.clone(),
                derive_served(
                    device.native,
                    device.label,
                    resolved.get(&device.key.id),
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
                device.native.device().functions().iter().any(|function| {
                    !memberships.contains_key(&FunctionKey::new(device.key.clone(), function.id()))
                })
            }
        })
        .map(|device| device.key.clone())
        .collect();
    let member_issues = resolved
        .into_iter()
        .filter_map(|(did, resolved)| {
            (!resolved.issues.is_empty()).then_some((did, resolved.issues))
        })
        .collect();
    DerivedConfiguration {
        served,
        published,
        groups: groups.clone(),
        memberships,
        member_issues,
        rules,
    }
}

/// Build one served logical description from canonical native facts and the
/// already resolved committed graph.
fn derive_served(
    native: &NativeDescription,
    label: &str,
    group: Option<&ResolvedMembers>,
    member_natives: &BTreeMap<String, &NativeDescription>,
) -> ServedDevice {
    let description = native.physical_description(label);
    let Some(resolved) = group else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: None,
        };
    };
    let Some(function) = native.device().unique_light() else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: Some("native group light is missing or ambiguous".into()),
        };
    };
    let light = function.lighting().expect("group light has bindings");
    let native_profile = light.profile();
    let native_rgb = light.color().is_some();
    let Some(native_interval) = light
        .temperature()
        .map(|temperature| temperature.interval())
    else {
        return ServedDevice {
            description,
            repair: None,
            repair_reason: (!native_rgb).then(|| "native group cannot be extended with RGB".into()),
        };
    };
    let members = resolved
        .members
        .iter()
        .map(|key| {
            let function = member_natives[&key.device.id]
                .device()
                .functions()
                .iter()
                .find(|function| function.id() == key.id)
                .expect("resolved member function exists");
            (key, function)
        })
        .collect::<Vec<_>>();
    let logical = if resolved.issues.is_empty() {
        members
            .iter()
            .map(|(_, function)| {
                function
                    .lighting()
                    .and_then(|light| light.temperature())
                    .map(|temperature| temperature.interval())
            })
            .collect::<Option<Vec<_>>>()
            .map(|intervals| {
                crate::xiaomi::capability::effective_temperature_range(native_interval, &intervals)
            })
            .unwrap_or(native_interval)
    } else {
        native_interval
    };
    let plan = if native_rgb {
        Ok(None)
    } else if let Some(issue) = resolved.issues.first() {
        Err(issue.clone())
    } else {
        crate::xiaomi::capability::plan_rgb_repair(native.device(), members)
    };
    let (repair, repair_reason, profile) = match plan {
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
        description: description.with_light_profile(function.id(), profile),
        repair,
        repair_reason,
    }
}

/// True when two native descriptions use the same bindings and input semantics.
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
                    && left.curtain() == right.curtain()
                    && left.fan() == right.fan()
                    && left.measurement() == right.measurement()
                    && left.state() == right.state()
                    && left.motion() == right.motion()
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
