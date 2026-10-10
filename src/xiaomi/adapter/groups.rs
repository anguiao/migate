use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, DeviceSlot},
};
use crate::{
    device::{Category, Composition, DeviceKey},
    runtime::default_device_label,
    storage::{CompositionCommitError, import_label},
    xiaomi::cloud::CloudGroup,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
};

/// The whole relation set one sync evaluation wants to persist, together with
/// the per-group diagnostics that explain deferred or revoked relations.
struct Evaluated {
    compositions: Vec<Composition>,
    /// Confirmed member deletions still referenced by the published set.
    removed: Vec<DeviceKey>,
    problems: Vec<(String, Option<String>)>,
}

/// Freshness of one identity's cloud relation in this directory read.
enum Fresh {
    /// No relation metadata is currently available for this identity.
    None,
    /// The read succeeded but carried no confirmed member relation.
    Unknown,
    /// A confirmed relation that cannot be published yet.
    Pending(String),
    /// A complete relation whose members are all supported single lights.
    Ready(Vec<DeviceKey>),
    /// A confirmed relation outside this round's supported shape.
    Rejected(String),
}

/// One known group identity's fresh and last confirmed relation.
struct Relation {
    fresh: Fresh,
    retained: Option<Composition>,
    deferred: Option<String>,
}

impl Context {
    /// Re-evaluate the whole relation set and persist it only when the
    /// evaluated snapshot differs from the published one. A failed commit
    /// leaves the previously confirmed registry state untouched.
    pub(super) fn reconcile_groups(self: &Rc<Self>) -> Result<(), AdapterError> {
        self.reconcile_groups_removed(&[])
    }

    /// Re-evaluate with a confirmed whole-home deletion set. Deletions are
    /// persisted and published atomically before any runtime resource is
    /// retired, so a failed commit leaves the previous topology intact.
    pub(super) fn reconcile_groups_removed(
        self: &Rc<Self>,
        removed: &[DeviceKey],
    ) -> Result<(), AdapterError> {
        let Some(scope) = self.live.borrow().scope.clone() else {
            return Ok(());
        };
        let evaluated = self.evaluate_groups(removed);
        for (did, reason) in &evaluated.problems {
            self.problem(did, reason.clone());
        }
        let published = self.registry.snapshot().composition.groups;
        if evaluated.compositions == published && evaluated.removed.is_empty() {
            return Ok(());
        }
        let confirmed = match self.store.devices().commit_composition(
            &scope,
            &evaluated.compositions,
            &evaluated.removed,
        ) {
            Ok(confirmed) => confirmed,
            Err(CompositionCommitError::Storage(error)) => return Err(error.into()),
            Err(error) => {
                if let Some(group) = evaluated.compositions.first() {
                    self.problem(&group.key.id, Some(error.to_string()));
                }
                return Ok(());
            }
        };
        let compositions = confirmed
            .groups
            .into_iter()
            .map(|group| Composition {
                key: group.key,
                label: group.label,
                members: group.members,
            })
            .collect::<Vec<_>>();
        self.registry
            .update_composition_versioned(confirmed.version, compositions, &evaluated.removed)
            .map_err(|error| AdapterError::Unavailable(error.to_string()))?;
        self.retire_confirmed_removed(&evaluated.removed);
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn evaluate_groups(&self, removed: &[DeviceKey]) -> Evaluated {
        let live = self.live.borrow();
        let deleted = removed
            .iter()
            .map(|key| key.id.as_str())
            .collect::<BTreeSet<_>>();
        let published = self.registry.snapshot().composition.groups;
        let mut identities = BTreeSet::new();
        for group in &published {
            identities.insert(group.key.id.clone());
        }
        for (did, slot) in &live.devices {
            // `group.*` is known at the DID stage; a numeric group model is
            // known once a previous directory read classified the identity.
            if slot.group.is_some() || did.starts_with("group.") {
                identities.insert(did.clone());
            }
        }
        let mut relations = BTreeMap::<String, Relation>::new();
        for id in &identities {
            let retained = published.iter().find(|group| &group.key.id == id).cloned();
            let fresh = if deleted.contains(id.as_str()) {
                Fresh::Rejected(format!("group {id} was removed from the selected home"))
            } else {
                match live
                    .devices
                    .get(id)
                    .and_then(|slot| slot.group.as_ref())
                    .and_then(|meta| meta.relation.as_ref())
                {
                    None => Fresh::None,
                    Some(CloudGroup::Unconfirmed) => Fresh::Unknown,
                    Some(CloudGroup::Confirmed { members, .. }) => fresh_relation(
                        id,
                        members,
                        &live.devices,
                        &deleted,
                        &identities,
                        live.complete,
                    ),
                }
            };
            relations.insert(
                id.clone(),
                Relation {
                    fresh,
                    retained,
                    deferred: None,
                },
            );
        }

        // Overlap is established by the raw confirmed membership, before any
        // member capability validation may reject a relation: a supported
        // group must not publish while another confirmed group claims the same
        // member, even if that other relation is otherwise unsupported. A
        // deleted parent identity is no longer a claim.
        let mut confirmed = Vec::new();
        for id in &identities {
            if deleted.contains(id.as_str()) {
                continue;
            }
            let Some(CloudGroup::Confirmed { members, .. }) = live
                .devices
                .get(id)
                .and_then(|slot| slot.group.as_ref())
                .and_then(|meta| meta.relation.as_ref())
            else {
                continue;
            };
            confirmed.push((
                id,
                members
                    .iter()
                    .map(|member| DeviceKey::new("xiaomi", member))
                    .collect::<Vec<_>>(),
            ));
        }
        let mut overlapping = BTreeSet::new();
        for (index, (left_id, left_members)) in confirmed.iter().enumerate() {
            for (right_id, right_members) in &confirmed[index + 1..] {
                if left_members
                    .iter()
                    .any(|member| right_members.contains(member))
                {
                    overlapping.insert((*left_id).clone());
                    overlapping.insert((*right_id).clone());
                }
            }
        }

        // Relations that stay authoritative reserve every member they still
        // reference; unknown or deferred owners protect old ownership before
        // any fresh candidate may claim the same member. Overlapping
        // identities are unsupported and reserve nothing.
        let mut authoritative = BTreeMap::<String, Composition>::new();
        let mut reserved = HashMap::<DeviceKey, String>::new();
        for (id, relation) in &relations {
            if overlapping.contains(id)
                || matches!(relation.fresh, Fresh::Rejected(_) | Fresh::Ready(_))
            {
                continue;
            }
            let Some(retained) = &relation.retained else {
                continue;
            };
            if retained_invalid(retained, &live.devices, &deleted, live.complete) {
                continue;
            }
            authoritative.insert(id.clone(), retained.clone());
            for member in &retained.members {
                reserved.entry(member.clone()).or_insert_with(|| id.clone());
            }
        }
        // Deferring a fresh relation keeps its retained relation alive, which
        // can reserve more members and defer further claims. Iterate to a
        // fixed point so transitive dependencies are honored.
        loop {
            let mut progressed = false;
            for (id, relation) in &mut relations {
                if overlapping.contains(id) {
                    continue;
                }
                let Fresh::Ready(members) = &relation.fresh else {
                    continue;
                };
                if relation.deferred.is_some() {
                    continue;
                }
                let conflict = members.iter().find_map(|member| {
                    reserved
                        .get(member)
                        .filter(|owner| *owner != id)
                        .map(|owner| (member, owner.clone()))
                });
                let Some((member, owner)) = conflict else {
                    continue;
                };
                relation.deferred = Some(format!(
                    "group relation for {id} is deferred while {owner} retains member {}",
                    member.id
                ));
                progressed = true;
                if let Some(retained) = &relation.retained
                    && !retained_invalid(retained, &live.devices, &deleted, live.complete)
                {
                    authoritative.insert(id.clone(), retained.clone());
                    for retained_member in &retained.members {
                        reserved
                            .entry(retained_member.clone())
                            .or_insert_with(|| id.clone());
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        let mut compositions = Vec::new();
        let mut problems = Vec::new();
        for (id, relation) in relations {
            let problem = match &relation.fresh {
                Fresh::Rejected(reason) => Some(reason.clone()),
                _ if overlapping.contains(&id) => {
                    Some(format!("group {id} overlaps another confirmed group"))
                }
                _ => relation.deferred.clone().or_else(|| match &relation.fresh {
                    Fresh::Pending(reason) => Some(reason.clone()),
                    Fresh::Unknown => Some(freshness_problem(
                        &live.devices,
                        &id,
                        "waits for a confirmed member relation",
                    )),
                    Fresh::None => Some(freshness_problem(
                        &live.devices,
                        &id,
                        "waits for a complete device description",
                    )),
                    _ => None,
                }),
            };
            let publishable = !matches!(relation.fresh, Fresh::Rejected(_))
                && !overlapping.contains(&id)
                && relation.deferred.is_none();
            match &relation.fresh {
                Fresh::Ready(members) if publishable => {
                    let label = relation
                        .retained
                        .as_ref()
                        .map(|group| group.label.clone())
                        .or_else(|| group_name(&live.devices, &id))
                        .unwrap_or_else(|| {
                            let model = live
                                .devices
                                .get(&id)
                                .and_then(|slot| slot.group.as_ref())
                                .map(|meta| meta.model.as_str())
                                .unwrap_or_default();
                            default_device_label(model, &id)
                        });
                    compositions.push(Composition {
                        key: DeviceKey::new("xiaomi", &id),
                        label,
                        members: members.clone(),
                    });
                }
                _ => {
                    // A confirmed overlap revokes retained ownership too; the
                    // relation is unsupported, not merely deferred.
                    if !overlapping.contains(&id)
                        && let Some(retained) = authoritative.get(&id)
                    {
                        compositions.push(retained.clone());
                    }
                }
            }
            problems.push((id, problem));
        }
        Evaluated {
            compositions,
            removed: removed.to_vec(),
            problems,
        }
    }
}

fn fresh_relation(
    id: &str,
    members: &[String],
    devices: &BTreeMap<String, DeviceSlot>,
    deleted: &BTreeSet<&str>,
    group_ids: &BTreeSet<String>,
    complete: bool,
) -> Fresh {
    if members.is_empty() {
        return Fresh::Rejected(format!("group {id} has no confirmed members"));
    }
    let mut unique = BTreeSet::new();
    let mut confirmed = Vec::new();
    let mut pending = Vec::new();
    for member in members {
        if member == id || !unique.insert(member.clone()) {
            return Fresh::Rejected(format!(
                "group {id} repeats member {member} or contains itself"
            ));
        }
        if group_ids.contains(member) {
            return Fresh::Rejected(format!("group {id} contains group {member} as a member"));
        }
        if deleted.contains(member.as_str()) {
            return Fresh::Rejected(format!(
                "group {id} still references deleted member {member}"
            ));
        }
        confirmed.push(DeviceKey::new("xiaomi", member));
        let Some(slot) = devices.get(member) else {
            if complete {
                return Fresh::Rejected(format!(
                    "group {id} references member {member} outside the selected home"
                ));
            }
            pending.push(member.clone());
            continue;
        };
        // A newly confirmed incompatible description outranks any older
        // admitted record that is no longer current.
        if slot.blocked_unsupported {
            return Fresh::Rejected(format!(
                "group {id} member {member} has an unsupported description"
            ));
        }
        // An uncertain replacement is not fresh evidence; keep the last
        // confirmed relation instead of publishing assumptions.
        if slot.loading.is_some() || slot.candidate.is_some() {
            pending.push(member.clone());
            continue;
        }
        match &slot.admitted {
            Some(record) if supported_member(record) => {}
            Some(_) => {
                return Fresh::Rejected(format!(
                    "group {id} member {member} is not a supported single light"
                ));
            }
            None => pending.push(member.clone()),
        }
    }
    if pending.is_empty() {
        Fresh::Ready(confirmed)
    } else {
        Fresh::Pending(format!(
            "group {id} waits for member descriptions: {}",
            pending.join(", ")
        ))
    }
}

/// A retained relation stops being authoritative once it references a member
/// that is confirmed deleted or confirmed unsupported.
fn retained_invalid(
    retained: &Composition,
    devices: &BTreeMap<String, DeviceSlot>,
    deleted: &BTreeSet<&str>,
    complete: bool,
) -> bool {
    retained.members.iter().any(|member| {
        if deleted.contains(member.id.as_str()) {
            return true;
        }
        match devices.get(&member.id) {
            None => complete,
            Some(slot) => {
                slot.blocked_unsupported
                    || slot
                        .admitted
                        .as_ref()
                        .is_some_and(|record| !supported_member(record))
            }
        }
    })
}

fn group_name(devices: &BTreeMap<String, DeviceSlot>, id: &str) -> Option<String> {
    match devices.get(id)?.group.as_ref()?.relation.as_ref()? {
        CloudGroup::Confirmed { name, .. } => name.as_deref().map(import_label),
        CloudGroup::Unconfirmed => None,
    }
}

/// Keep the most specific available reason: a transport failure recorded by
/// the latest read outranks the generic freshness explanation.
fn freshness_problem(devices: &BTreeMap<String, DeviceSlot>, id: &str, fallback: &str) -> String {
    devices
        .get(id)
        .and_then(|slot| slot.blocked.clone())
        .unwrap_or_else(|| format!("group {id} {fallback}"))
}

/// A supported group member is exactly one valid main light function and is
/// never a known group identity, matching the storage compatibility rule.
fn supported_member(record: &DeviceRecord) -> bool {
    let cached = record.cached.borrow();
    !crate::xiaomi::capability::is_light_group(&cached.device.key, &cached.model)
        && cached.device.functions.len() == 1
        && cached.device.functions[0].description.id == "main"
        && cached.device.functions[0].description.category == Category::Light
        && cached.device.functions[0].description.valid_capabilities()
}
