//! Independent confirmation of native group relations.
//!
//! Each group's relation is confirmed separately from the complete account
//! catalog. A present record whose membership is missing or malformed is a
//! read failure that keeps the last confirmed relation; only a confirmed empty
//! relation withdraws the published group, and only absence from the complete
//! directory deletes it. `group_did` is authoritative and may differ from the
//! outer record DID used to fetch details.

use crate::{
    configuration::{GroupRelation, Scope},
    xiaomi::cloud::{
        CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
    },
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConfirmedGroup {
    pub(super) membership: CloudGroupMembership,
    pub(super) scope: Scope,
    /// Outer catalog record this relation was read from, retained so a later
    /// present-but-failed record is never mistaken for a deleted group.
    pub(super) record_did: String,
    pub(super) model: String,
    pub(super) instance_type: Option<String>,
    /// Optional catalog name, retained as display metadata only.
    pub(super) name: Option<String>,
    /// Online observation from the same authoritative catalog record.
    pub(super) online: bool,
    /// True while the relation publishes; a confirmed empty or out-of-home
    /// relation withdraws the endpoint but keeps the reserved identity.
    pub(super) active: bool,
    pub(super) error: Option<String>,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub(super) struct RelationUpdate {
    pub(super) active: Vec<ConfirmedGroup>,
    pub(super) withdrawn: Vec<String>,
    pub(super) errors: Vec<(String, String)>,
}

#[derive(Default)]
pub(super) struct Relations {
    groups: BTreeMap<String, ConfirmedGroup>,
}

impl Relations {
    pub(super) fn snapshot(&self) -> Vec<ConfirmedGroup> {
        self.groups.values().cloned().collect()
    }

    /// True while a native fetch still matches the latest confirmed relation.
    pub(super) fn matches_fetch(
        &self,
        group_did: &str,
        record_did: &str,
        model: &str,
        instance_type: &str,
    ) -> bool {
        self.groups.get(group_did).is_some_and(|group| {
            group.active
                && group.record_did == record_did
                && group.instance_type.as_deref() == Some(instance_type)
                && group.model == model
        })
    }

    pub(super) fn restored(scope: &Scope, relations: &[GroupRelation]) -> Self {
        Self {
            groups: relations
                .iter()
                .map(|relation| {
                    (
                        relation.group_did.clone(),
                        ConfirmedGroup {
                            membership: CloudGroupMembership {
                                group_did: relation.group_did.clone(),
                                members: relation.members.clone(),
                                is_physical: relation.is_physical,
                            },
                            scope: scope.clone(),
                            record_did: relation.record_did.clone(),
                            model: relation.model.clone(),
                            instance_type: relation.instance_type.clone(),
                            name: None,
                            online: false,
                            active: relation.active,
                            error: relation.error.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Merge one complete account catalog into the confirmed relations.
    /// `home` is the selected home's ordinary DID set; a group is in scope
    /// when it has at least one canonical member and at least one of those
    /// members belongs to that home. Members outside the selected home stay in
    /// the confirmed list; only a fully-outside group is inactive.
    pub(super) fn apply(
        &mut self,
        catalog: &CloudDeviceCatalog,
        scope: &Scope,
        home: &BTreeSet<String>,
    ) -> RelationUpdate {
        let before: BTreeSet<String> = self.active_keys();
        let mut confirmations: BTreeMap<String, (CloudGroupMembership, &CloudCatalogDevice)> =
            BTreeMap::new();
        let mut records: BTreeMap<&str, &CloudCatalogDevice> = BTreeMap::new();
        for device in &catalog.devices {
            records.insert(device.did.as_str(), device);
            if let CloudGroupStatus::Confirmed(membership) = &device.group {
                confirmations.insert(membership.group_did.clone(), (membership.clone(), device));
            }
        }
        let retain_previous = |group: &ConfirmedGroup| {
            records.contains_key(group.record_did.as_str())
                && group.scope.uid == scope.uid
                && group.scope.home_id == scope.home_id
        };
        let mut errors = Vec::new();
        let nested: BTreeSet<String> = confirmations
            .iter()
            .filter(|(_, (membership, _))| {
                membership.members.iter().any(|member| {
                    member != &membership.group_did
                        && (confirmations.contains_key(member)
                            || self.groups.get(member).is_some_and(retain_previous))
                })
            })
            .map(|(did, _)| did.clone())
            .collect();
        for (group_did, (membership, record)) in &confirmations {
            let previous = self.groups.get(group_did);
            let empty = membership.members.is_empty();
            if empty && previous.is_none() {
                // An empty relation cannot establish a new group's scope.
                continue;
            }
            let error = if empty {
                None
            } else if nested.contains(group_did) {
                Some("nested group members are not supported".to_owned())
            } else if !membership
                .members
                .iter()
                .any(|member| home.contains(member))
            {
                Some("group members are outside the selected home".to_owned())
            } else {
                None
            };
            if let Some(error) = &error {
                errors.push((group_did.clone(), error.clone()));
            }
            let active = !empty && error.is_none();
            // Only a successful in-home confirmation establishes a new scope.
            let scope = if active {
                scope.clone()
            } else {
                previous.map(|group| &group.scope).unwrap_or(scope).clone()
            };
            self.groups.insert(
                group_did.clone(),
                ConfirmedGroup {
                    membership: membership.clone(),
                    scope,
                    record_did: record.did.clone(),
                    model: record.model.clone(),
                    instance_type: record.instance_type.clone(),
                    name: record.name.clone(),
                    online: record.online,
                    active,
                    error,
                },
            );
        }
        // Present records that no longer carry a confirmable member list keep
        // the last relation. Absence from the complete directory deletes it.
        let mut absent = Vec::new();
        for (group_did, group) in self.groups.iter_mut() {
            if confirmations.contains_key(group_did) {
                continue;
            }
            if retain_previous(group) {
                let reason = "group membership is missing or invalid for a present record";
                // Membership confirmation failed, but a present record's own
                // trusted access metadata still updates; the confirmed member
                // list is retained untouched.
                if let Some(record) = records.get(group.record_did.as_str()).copied() {
                    group.model = record.model.clone();
                    group.instance_type = record.instance_type.clone();
                    group.name = record.name.clone();
                    group.online = record.online;
                }
                group.error = Some(reason.into());
                errors.push((group_did.clone(), reason.into()));
            } else {
                absent.push(group_did.clone());
            }
        }
        for group_did in &absent {
            self.groups.remove(group_did);
        }
        let after = self.active_keys();
        let withdrawn = before.difference(&after).cloned().collect();
        RelationUpdate {
            active: self
                .groups
                .values()
                .filter(|group| group.active)
                .cloned()
                .collect(),
            withdrawn,
            errors,
        }
    }

    fn active_keys(&self) -> BTreeSet<String> {
        self.groups
            .values()
            .filter(|group| group.active)
            .map(|group| group.membership.group_did.clone())
            .collect()
    }
}
