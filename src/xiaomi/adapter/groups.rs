//! Native group sources: confirmed relations become real controllable sources.
//!
//! A group is admitted only from a confirmed, active catalog relation. Its own
//! MIoT instance is fetched and parsed exactly like an ordinary light; member
//! descriptions are never required. Identity and publication are committed by
//! the runtime configuration boundary. Virtual groups
//! are cloud-only; physical groups keep their own DID's real route.

use super::{model::AdapterError, relations::ConfirmedGroup, state::Context};
use crate::{
    device::DeviceKey,
    xiaomi::capability::RawDescription,
    xiaomi::cloud::{CloudClient, CloudDeviceInfo},
};
use futures_lite::future;
use std::rc::Rc;

impl Context {
    /// A group runtime is installed by the unified configuration commit; this
    /// only refreshes the serving access conditions.
    pub(super) fn reconcile_group(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        self.install_work(did);
        self.update_availability(did);
        Ok(())
    }

    /// Retire a group source without touching persisted ordinary identity.
    pub(super) fn remove_group(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        // Clear the retained physical view and retire the group's own native
        // subscriptions through the existing source mechanisms before the
        // slot disappears.
        self.revoke(did, "native group withdrawn")?;
        self.remove_source_slot(did);
        Ok(())
    }

    /// Reconcile confirmed active relations into group slots. Stale group
    /// slots are retired; each active group gets its catalog metadata and, when
    /// needed, one owned fetch of its own native instance. A missing group
    /// instance type or fetch failure stays diagnostic and never invents a
    /// source.
    pub(super) fn sync_groups(
        self: &Rc<Self>,
        cloud: &CloudClient,
        token: &str,
        active: &[ConfirmedGroup],
    ) -> Result<(), AdapterError> {
        let wanted = active
            .iter()
            .map(|group| group.membership.group_did.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let stale = self
            .live
            .borrow()
            .devices
            .iter()
            .filter(|(did, slot)| slot.group && !wanted.contains(*did))
            .map(|(did, _)| did.clone())
            .collect::<Vec<_>>();
        for did in stale {
            self.remove_group(&did)?;
        }
        for group in active {
            let did = &group.membership.group_did;
            let existing = self
                .live
                .borrow()
                .devices
                .get(did)
                .is_some_and(|slot| slot.group);
            let known = if existing {
                self.graph.borrow().groups.get(did).map(|known| {
                    (
                        known.is_physical,
                        known.model.clone(),
                        known.instance_type.clone(),
                    )
                })
            } else {
                None
            };
            let recovered_target = existing
                && self.live.borrow().devices[did].description_target_missing
                && group.instance_type.is_some();
            let needs_local = {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.entry(did.clone()).or_default();
                slot.confirmed = true;
                // Retained relations with a read error cannot restore revoked access.
                if group.error.is_none() {
                    slot.cloud_forbidden = false;
                }
                slot.group = true;
                slot.description_target_missing = group.instance_type.is_none();
                slot.cloud_access =
                    group
                        .instance_type
                        .as_ref()
                        .map(|instance_type| CloudDeviceInfo {
                            did: did.clone(),
                            model: group.model.clone(),
                            instance_type: instance_type.clone(),
                            online: group.online,
                        });
                group.membership.is_physical
                    && known.is_none_or(|(physical, model, instance_type)| {
                        !physical || model != group.model || instance_type != group.instance_type
                    })
            };
            {
                let target = group
                    .instance_type
                    .as_ref()
                    .map(|instance_type| (group.model.clone(), instance_type.clone()));
                self.live
                    .borrow_mut()
                    .devices
                    .get_mut(did)
                    .unwrap()
                    .retarget_loading(&target);
            }
            if recovered_target {
                self.clear_description_problem(did);
            }
            if needs_local {
                let access = self
                    .live
                    .borrow()
                    .session
                    .as_ref()
                    .map(|session| session.access.clone());
                if let Some(access) = access {
                    access.request_filtered(vec![did.clone()]);
                }
            }
            let Some(instance_type) = group.instance_type.clone() else {
                self.problem(
                    did,
                    Some("native group description has no instance type".into()),
                );
                continue;
            };
            if !self.live.borrow().devices[did].needs_description() {
                self.resolved_description_target(did);
                self.reconcile_group(did)?;
                continue;
            }
            let life = {
                let mut live = self.live.borrow_mut();
                let target = Some((group.model.clone(), instance_type.clone()));
                live.devices.get_mut(did).unwrap().start_loading(target)
            };
            let context = self.clone();
            let client = cloud.clone();
            let token = token.to_string();
            let model = group.model.clone();
            let record_did = group.record_did.clone();
            let group_did = did.clone();
            let name = group.name.clone();
            let scope = group.scope.clone();
            let captured_scope = scope.clone();
            self.tasks.spawn(async move {
                // A superseded or removed target retires this fetch as well
                // as its loading token.
                let wait_life = life.clone();
                let result = future::or(
                    async { Some(client.get_instance(&instance_type).await) },
                    future::or(
                        async {
                            context.token_retired(&token).await;
                            None
                        },
                        async {
                            wait_life.ended().await;
                            None
                        },
                    ),
                )
                .await;
                let cleared = {
                    let mut live = context.live.borrow_mut();
                    let Some(slot) = live.devices.get_mut(&group_did) else {
                        return Ok::<(), AdapterError>(());
                    };
                    slot.finish_loading(&life)
                };
                if !cleared {
                    return Ok::<(), AdapterError>(());
                }
                let Some(result) = result else {
                    return Ok(());
                };
                let current = context.current_token(&token)
                    && context.live.borrow().scope.as_ref().is_some_and(|scope| {
                        scope.uid == captured_scope.uid && scope.home_id == captured_scope.home_id
                    })
                    && context.relations.borrow().matches_fetch(
                        &group_did,
                        &record_did,
                        &model,
                        &instance_type,
                    );
                if !current {
                    return Ok::<(), AdapterError>(());
                }
                match result {
                    Ok(instance) => {
                        match RawDescription::new(
                            DeviceKey::new("xiaomi", group_did.clone()),
                            model,
                            instance_type,
                            instance,
                            None,
                        )
                        .and_then(|raw| raw.recognize(true))
                        {
                            Ok((raw, native)) => {
                                if let Some(slot) =
                                    context.live.borrow_mut().devices.get_mut(&group_did)
                                {
                                    slot.blocked = None;
                                    slot.recompute_failed = false;
                                    slot.latest = native;
                                    slot.raw = Some(raw);
                                    slot.latest_name = name;
                                }
                                context.clear_description_problem(&group_did);
                                context.stage();
                            }
                            Err(error) => {
                                context.pending_description(&group_did, error.to_string());
                            }
                        }
                    }
                    Err(error) => {
                        context.pending_description(&group_did, error.to_string());
                        if error.is_unauthorized() {
                            context.cloud_error(&token, None, &error)?;
                        }
                    }
                }
                Ok::<(), AdapterError>(())
            });
            self.changed.notify(usize::MAX);
        }
        Ok(())
    }
}
