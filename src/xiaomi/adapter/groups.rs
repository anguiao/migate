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
use std::collections::{BTreeMap, BTreeSet};

/// The whole relation set a sync evaluation wants to persist.
struct Evaluated {
    compositions: Vec<Composition>,
    /// Confirmed member deletions still referenced by the published set.
    removed: Vec<DeviceKey>,
}

/// What one known group identity contributes to the evaluated relation set.
enum Decision {
    /// The freshly confirmed relation is complete and may be published.
    Candidate(Composition),
    /// The last confirmed relation stays published while the cloud is unclear.
    Preserve(Composition),
    /// Nothing is published for this identity and nothing prior needs revoking.
    Skip,
    /// The relation is confirmed invalid or its members are unsupported.
    Revoke,
}

impl Context {
    /// Re-evaluate the whole relation set and persist it only when the
    /// evaluated snapshot differs from the published one. A failed commit
    /// leaves the previously confirmed registry state untouched.
    pub(super) fn reconcile_groups(&self) -> Result<(), AdapterError> {
        let Some(scope) = self.live.borrow().scope.clone() else {
            return Ok(());
        };
        let evaluated = self.evaluate_groups();
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
        self.changed.notify(usize::MAX);
        Ok(())
    }

    fn evaluate_groups(&self) -> Evaluated {
        let live = self.live.borrow();
        let published = self.registry.snapshot().composition.groups;
        let mut decisions = BTreeMap::<String, Decision>::new();
        for (did, slot) in &live.devices {
            let Some(meta) = &slot.group else {
                continue;
            };
            let retained = published.iter().find(|group| &group.key.id == did);
            decisions.insert(
                did.clone(),
                decision(meta, &live.devices, retained, live.complete),
            );
        }
        for group in &published {
            decisions.entry(group.key.id.clone()).or_insert_with(|| {
                if live.devices.contains_key(&group.key.id) || !live.complete {
                    Decision::Preserve(group.clone())
                } else {
                    Decision::Revoke
                }
            });
        }
        let mut compositions = Vec::new();
        for decision in decisions.values() {
            match decision {
                Decision::Candidate(group) | Decision::Preserve(group) => {
                    compositions.push(group.clone());
                }
                Decision::Skip | Decision::Revoke => {}
            }
        }
        let mut removed = Vec::new();
        if live.complete {
            for group in &published {
                for member in &group.members {
                    if !live.devices.contains_key(&member.id) && !removed.contains(member) {
                        removed.push(member.clone());
                    }
                }
            }
        }
        Evaluated {
            compositions,
            removed,
        }
    }
}

fn decision(
    meta: &super::state::GroupMeta,
    devices: &BTreeMap<String, DeviceSlot>,
    retained: Option<&Composition>,
    complete: bool,
) -> Decision {
    match &meta.group {
        CloudGroup::Unconfirmed => {
            retained.map_or(Decision::Skip, |group| Decision::Preserve(group.clone()))
        }
        CloudGroup::Confirmed {
            did, name, members, ..
        } => {
            if members.is_empty() || members.iter().any(|member| member == did) {
                return Decision::Revoke;
            }
            let mut unique = BTreeSet::new();
            if members.iter().any(|member| !unique.insert(member)) {
                return Decision::Revoke;
            }
            let mut pending = false;
            for member in members {
                let Some(member_slot) = devices.get(member) else {
                    // Outside a complete directory read, absence is not deletion.
                    if complete {
                        return Decision::Revoke;
                    }
                    pending = true;
                    continue;
                };
                // A newly confirmed incompatible description outranks any
                // older admitted record that is no longer current.
                if member_slot.blocked_unsupported {
                    return Decision::Revoke;
                }
                // An uncertain replacement is not fresh evidence; keep the
                // last confirmed relation instead of publishing assumptions.
                if member_slot.loading.is_some() || member_slot.candidate.is_some() {
                    pending = true;
                    continue;
                }
                if let Some(record) = &member_slot.admitted {
                    if supported_member(record) {
                        continue;
                    }
                    return Decision::Revoke;
                }
                pending = true;
            }
            if pending {
                return retained.map_or(Decision::Skip, |group| Decision::Preserve(group.clone()));
            }
            let label = retained
                .map(|group| group.label.clone())
                .or_else(|| name.as_deref().map(import_label))
                .unwrap_or_else(|| default_device_label(&meta.model, did));
            Decision::Candidate(Composition {
                key: DeviceKey::new("xiaomi", did),
                label,
                members: members
                    .iter()
                    .map(|member| DeviceKey::new("xiaomi", member))
                    .collect(),
            })
        }
    }
}

/// A supported group member is exactly one main light function and is never a
/// known group identity, matching the storage compatibility rule.
fn supported_member(record: &DeviceRecord) -> bool {
    let cached = record.cached.borrow();
    !crate::xiaomi::capability::is_light_group(&cached.device.key, &cached.model)
        && cached.device.functions.len() == 1
        && cached.device.functions[0].description.id == "main"
        && cached.device.functions[0].description.category == Category::Light
}
