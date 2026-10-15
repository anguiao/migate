//! Independent confirmation of native group relations.
//!
//! Each group's relation is confirmed separately from the complete account
//! catalog. A present record whose membership is missing or malformed is a
//! read failure that keeps the last confirmed relation; only a confirmed empty
//! relation withdraws the published group, and only absence from the complete
//! directory deletes it. `group_did` is authoritative and may differ from the
//! outer record DID used to fetch details.

use crate::{
    device::DeviceKey,
    storage::CachedScope,
    xiaomi::cloud::{
        CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
    },
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConfirmedGroup {
    pub(super) membership: CloudGroupMembership,
    pub(super) scope: CachedScope,
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
    /// Merge one complete account catalog into the confirmed relations.
    /// `home` is the selected home's ordinary DID set; a group is in scope
    /// when it has at least one member and every member belongs to that home.
    pub(super) fn apply(
        &mut self,
        catalog: &CloudDeviceCatalog,
        scope: &CachedScope,
        home: &BTreeSet<String>,
    ) -> RelationUpdate {
        let before: BTreeSet<String> = self.active_keys();
        let mut present_records = BTreeSet::new();
        let mut confirmations: BTreeMap<String, (CloudGroupMembership, &CloudCatalogDevice)> =
            BTreeMap::new();
        let mut records: BTreeMap<&str, &CloudCatalogDevice> = BTreeMap::new();
        for device in &catalog.devices {
            present_records.insert(device.did.clone());
            records.insert(device.did.as_str(), device);
            if let CloudGroupStatus::Confirmed(membership) = &device.group {
                confirmations.insert(membership.group_did.clone(), (membership.clone(), device));
            }
        }
        let mut errors = Vec::new();
        let nested: BTreeSet<String> = confirmations
            .iter()
            .filter(|(_, (membership, _))| {
                membership.members.iter().any(|member| {
                    confirmations.contains_key(member) && member != &membership.group_did
                })
            })
            .map(|(did, _)| did.clone())
            .collect();
        for (group_did, (membership, record)) in &confirmations {
            let previous = self.groups.get(group_did).cloned();
            let record_did = &record.did;
            // The confirmation carries the actual catalog record: no re-find
            // and no manufactured empty metadata.
            let model = record.model.clone();
            let instance_type = record.instance_type.clone();
            let name = record.name.clone();
            let online = record.online;
            if membership.members.is_empty() {
                // A confirmed empty relation withdraws a previously scoped
                // group. A first-time empty relation cannot establish scope.
                let Some(previous) = previous else {
                    continue;
                };
                self.groups.insert(
                    group_did.clone(),
                    ConfirmedGroup {
                        membership: membership.clone(),
                        scope: previous.scope,
                        record_did: record_did.clone(),
                        model,
                        instance_type,
                        name,
                        online,
                        active: false,
                        error: None,
                    },
                );
                continue;
            }
            if nested.contains(group_did) {
                let reason = "nested group members are not supported".to_string();
                errors.push((group_did.clone(), reason.clone()));
                self.groups.insert(
                    group_did.clone(),
                    ConfirmedGroup {
                        membership: membership.clone(),
                        scope: previous
                            .map(|group| group.scope)
                            .unwrap_or_else(|| scope.clone()),
                        record_did: record_did.clone(),
                        model,
                        instance_type,
                        name,
                        online,
                        active: false,
                        error: Some(reason),
                    },
                );
                continue;
            }
            if !membership
                .members
                .iter()
                .all(|member| home.contains(member))
            {
                let reason = "group members are outside the selected home".to_string();
                errors.push((group_did.clone(), reason.clone()));
                self.groups.insert(
                    group_did.clone(),
                    ConfirmedGroup {
                        membership: membership.clone(),
                        scope: previous
                            .map(|group| group.scope)
                            .unwrap_or_else(|| scope.clone()),
                        record_did: record_did.clone(),
                        model,
                        instance_type,
                        name,
                        online,
                        active: false,
                        error: Some(reason),
                    },
                );
                continue;
            }
            // A successful in-home confirmation belongs to the current scope;
            // a stale previous scope must not suppress the native fetch.
            self.groups.insert(
                group_did.clone(),
                ConfirmedGroup {
                    membership: membership.clone(),
                    scope: scope.clone(),
                    record_did: record_did.clone(),
                    model,
                    instance_type,
                    name,
                    online,
                    active: true,
                    error: None,
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
            if present_records.contains(&group.record_did)
                && group.scope.uid == scope.uid
                && group.scope.home_id == scope.home_id
            {
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
        let withdrawn = before
            .difference(&after)
            .chain(absent.iter().filter(|did| before.contains(*did)))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
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

    /// Raw member observers for virtual groups only, keyed by member DID.
    pub(super) fn member_watches(&self) -> BTreeMap<String, BTreeSet<DeviceKey>> {
        let mut watches = BTreeMap::new();
        for group in self.groups.values().filter(|group| group.active) {
            if group.membership.is_physical {
                continue;
            }
            for member in &group.membership.members {
                watches
                    .entry(member.clone())
                    .or_insert_with(BTreeSet::new)
                    .insert(DeviceKey::new("xiaomi", group.membership.group_did.clone()));
            }
        }
        watches
    }
}
