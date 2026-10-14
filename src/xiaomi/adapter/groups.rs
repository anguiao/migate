use super::{
    model::AdapterError,
    state::{Context, DeviceRecord, DeviceSlot},
};
use crate::{
    device::{
        Category, Composition, ConfigurationScope, DeviceKey, FunctionDescription,
        derive_group_capabilities,
    },
    runtime::default_device_label,
    storage::import_label,
    xiaomi::cloud::CloudGroup,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

/// The whole relation set one sync evaluation wants to persist, together with
/// the per-group diagnostics that explain pending or revoked relations.
struct Evaluated {
    compositions: Vec<Composition>,
    problems: Vec<(String, Option<String>)>,
}

/// Freshness of one identity's cloud relation in this directory read.
enum Fresh {
    /// A relation that cannot be published yet, with its latest diagnostic.
    Pending(String),
    /// A complete relation whose members are all supported single lights.
    Ready(Vec<DeviceKey>),
    /// A confirmed relation outside this round's supported shape.
    Rejected(String),
}

impl Context {
    /// Re-evaluate the whole relation set and stage the latest confirmed
    /// result for the configuration apply boundary. Confirmed deletions revoke real
    /// control immediately; nothing candidate is persisted as applied here.
    pub(super) fn reconcile_groups(self: &Rc<Self>) -> Result<(), AdapterError> {
        self.reconcile_groups_removed(&[])
    }

    /// Re-evaluate with a confirmed whole-home deletion set. Every deleted
    /// member's control path, consumer and source reference is revoked
    /// immediately, before and regardless of any storage attempt, so a failed
    /// publication commit can never retain control over it. The surviving
    /// relation set is only staged; storage and the applied view change
    /// together at the configuration apply boundary.
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
        for key in removed {
            self.registry.revoke(key);
        }
        self.retire_confirmed_removed(removed);
        // The applied storage keeps the last committed topology, derived shape,
        // publication set and versions until the configuration commit succeeds. Only
        // real control is revoked here; a durable removal is part of the same
        // atomic configuration commit as everything else.
        self.registry
            .stage_configuration(
                ConfigurationScope {
                    uid: scope.uid,
                    home_id: scope.home_id,
                    group_id: scope.group_id,
                },
                evaluated.compositions,
                removed.to_vec(),
            )
            .map_err(|error| AdapterError::Unavailable(error.to_string()))?;
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn evaluate_groups(&self, removed: &[DeviceKey]) -> Evaluated {
        let live = self.live.borrow();
        let deleted = removed
            .iter()
            .map(|key| key.id.as_str())
            .collect::<BTreeSet<_>>();
        // The latest confirmed relation set is the retention baseline: a
        // newer pending relation must survive a later failed directory read
        // instead of falling back to the older applied version.
        let published = self.registry.desired_composition();
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
        let mut compositions = Vec::new();
        let mut problems = Vec::new();
        for id in &identities {
            let retained = published.iter().find(|group| &group.key.id == id);
            let fresh = if deleted.contains(id.as_str()) {
                Fresh::Rejected(format!("group {id} was removed from the selected home"))
            } else {
                match live
                    .devices
                    .get(id)
                    .and_then(|slot| slot.group.as_ref())
                    .and_then(|meta| meta.relation.as_ref())
                {
                    None => Fresh::Pending(freshness_problem(
                        &live.devices,
                        id,
                        "waits for a complete device description",
                    )),
                    Some(CloudGroup::Unconfirmed) => Fresh::Pending(freshness_problem(
                        &live.devices,
                        id,
                        "waits for a confirmed member relation",
                    )),
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
            let problem = match fresh {
                Fresh::Ready(members) => {
                    let label = retained
                        .map(|group| group.label.clone())
                        .or_else(|| group_name(&live.devices, id))
                        .unwrap_or_else(|| {
                            let model = live
                                .devices
                                .get(id)
                                .and_then(|slot| slot.group.as_ref())
                                .map(|meta| meta.model.as_str())
                                .unwrap_or_default();
                            default_device_label(model, id)
                        });
                    compositions.push(Composition {
                        key: DeviceKey::new("xiaomi", id),
                        label,
                        members,
                    });
                    None
                }
                Fresh::Pending(reason) => {
                    // Each group retains only its own latest confirmation.
                    // A shared member never blocks another group's progress.
                    if let Some(retained) = retained
                        && !retained_invalid(retained, &live.devices, &deleted, live.complete)
                    {
                        compositions.push(retained.clone());
                    }
                    Some(reason)
                }
                Fresh::Rejected(reason) => Some(reason),
            };
            problems.push((id.clone(), problem));
        }
        Evaluated {
            compositions,
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
    let mut descriptions = Vec::new();
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
        match &slot.admitted {
            Some(record) => {
                if let Some(problem) = member_problem(record) {
                    return Fresh::Rejected(format!(
                        "group {id} member {member} cannot be published standalone: {problem}"
                    ));
                }
                descriptions.push(record.cached.borrow());
            }
            None => pending.push(member.clone()),
        }
    }
    if !pending.is_empty() {
        return Fresh::Pending(format!(
            "group {id} waits for member descriptions: {}",
            pending.join(", ")
        ));
    }
    // The derived shape is admission-checked through the same mapping rule as
    // a single light: a non-empty common range that Matter mireds cannot
    // represent makes the group unsupported instead of fabricating another
    // capability set. Members then stay standalone candidates.
    let description = FunctionDescription {
        id: "main".into(),
        label: String::new(),
        category: Category::Light,
        capabilities: derive_group_capabilities(
            descriptions
                .iter()
                .map(|cached| &cached.device.functions[0].description),
        ),
    };
    if let Err(reason) = crate::matter::publication::map_light(&description) {
        return Fresh::Rejected(format!(
            "group {id} cannot be published: {}",
            reason.as_str()
        ));
    }
    Fresh::Ready(confirmed)
}

/// A retained relation stops being authoritative once it references a member
/// that is confirmed deleted or cannot be published as a standalone light.
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
                slot.group.is_some()
                    || slot
                        .admitted
                        .as_ref()
                        .is_some_and(|record| member_problem(record).is_some())
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

/// The reason a device record cannot be published as a standalone group
/// member. A valid but Matter-unmappable light is distinct from a temporary
/// transport or parse failure, which keeps the last valid record.
fn member_problem(record: &DeviceRecord) -> Option<String> {
    let cached = record.cached.borrow();
    if crate::xiaomi::capability::is_light_group(&cached.device.key, &cached.model) {
        return Some("is a light-group identity".into());
    }
    if cached.device.functions.len() != 1
        || cached.device.functions[0].description.id != "main"
        || cached.device.functions[0].description.category != Category::Light
        || !cached.device.functions[0].description.valid_capabilities()
    {
        return Some("is not a supported single light".into());
    }
    super::membership::publication_problem(&cached.device)
}
