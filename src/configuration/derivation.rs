use super::{CommittedConfiguration, DerivedConfiguration, GroupRelation, GroupRule, ServedDevice};
use crate::{
    device::{DeviceKey, FunctionDescription, FunctionKey},
    xiaomi::capability::NativeDescription,
};
use std::collections::{BTreeMap, BTreeSet};

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
}

fn relations_by_did(relations: &[GroupRelation]) -> BTreeMap<String, GroupRelation> {
    relations
        .iter()
        .map(|relation| (relation.group_did.clone(), relation.clone()))
        .collect()
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
    let function = native
        .device()
        .unique_light()
        .expect("a confirmed native group has one light");
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
    let composed = crate::xiaomi::capability::compose_group_light(
        function,
        &members,
        resolved.issues.first().map(String::as_str),
    );
    ServedDevice {
        description: description.with_light_profile(function.id(), composed.profile),
        repair: composed.repair,
        repair_reason: composed.reason,
    }
}
