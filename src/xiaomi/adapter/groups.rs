//! Native group sources: confirmed relations become real controllable sources.
//!
//! A group is admitted only from a confirmed, active catalog relation. Its own
//! MIoT instance is fetched and parsed exactly like an ordinary light; member
//! descriptions are never required. Identity and publication are committed by
//! the runtime configuration boundary. Virtual groups
//! are cloud-only; physical groups keep their own DID's real route.

use super::{model::AdapterError, relations::ConfirmedGroup, state::Context};
use crate::{device::DeviceKey, xiaomi::capability::RawDescription, xiaomi::cloud::CloudClient};
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
        token: &super::routing::CloudPath,
        active: &[ConfirmedGroup],
    ) -> Result<(), AdapterError> {
        let wanted = active
            .iter()
            .map(|group| group.membership.group_did.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let stale = self
            .directory
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
                .directory
                .borrow()
                .devices
                .get(did)
                .is_some_and(|slot| slot.group);
            let needs_local = group.membership.is_physical
                && (!existing
                    || self.graph.borrow().groups.get(did).is_none_or(|known| {
                        !known.is_physical
                            || known.model != group.model
                            || known.instance_type != group.instance_type
                    }));
            let (recovered_target, retired) = self.directory.borrow_mut().confirm_group(group);
            if let Some(life) = retired {
                life.close();
            }
            if recovered_target {
                self.clear_description_problem(did);
            }
            if needs_local {
                let access = self
                    .hub
                    .borrow()
                    .session()
                    .map(|session| session.access.clone());
                if let Some(access) = access {
                    access.request_filtered(vec![did.clone()]);
                }
            }
            let Some(instance_type) = group.instance_type.clone() else {
                self.pending_description(
                    did,
                    "native group description has no instance type".into(),
                );
                continue;
            };
            if !self.directory.borrow().devices[did].needs_description() {
                self.resolved_description_target(did);
                self.reconcile_group(did)?;
                continue;
            }
            let life = {
                let mut live = self.directory.borrow_mut();
                let target = Some((group.model.clone(), instance_type.clone()));
                live.devices.get_mut(did).unwrap().start_loading(target)
            };
            let context = self.clone();
            let client = cloud.clone();
            let token = token.clone();
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
                    async {
                        context.mijia_retired(&token).await;
                        None
                    },
                    future::or(
                        async {
                            wait_life.ended().await;
                            None
                        },
                        async { Some(client.get_instance(&instance_type).await) },
                    ),
                )
                .await;
                let cleared = {
                    let mut live = context.directory.borrow_mut();
                    let Some(slot) = live.devices.get_mut(&group_did) else {
                        return Ok::<(), AdapterError>(());
                    };
                    slot.finish_loading(&life)
                };
                if !cleared {
                    return Ok::<(), AdapterError>(());
                }
                let Some(result) = result else {
                    context.request_sync();
                    return Ok(());
                };
                let current = context.current_mijia(&token)
                    && context
                        .directory
                        .borrow()
                        .scope
                        .as_ref()
                        .is_some_and(|scope| {
                            scope.uid == captured_scope.uid
                                && scope.home_id == captured_scope.home_id
                        })
                    && context.directory.borrow().matches_group_fetch(
                        &group_did,
                        &record_did,
                        &model,
                        &instance_type,
                    );
                if !current {
                    context.request_sync();
                    return Ok::<(), AdapterError>(());
                }
                match result {
                    Ok(instance) => {
                        match RawDescription::new(
                            DeviceKey::new("xiaomi", group_did.clone()),
                            model,
                            name,
                            instance_type,
                            instance,
                            None,
                        )
                        .and_then(|raw| raw.recognize(true))
                        {
                            Ok(description) => {
                                context
                                    .directory
                                    .borrow_mut()
                                    .accept_description(&group_did, description);
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
                    }
                }
                Ok::<(), AdapterError>(())
            });
            self.changed.notify(usize::MAX);
        }
        Ok(())
    }
}
