//! Native group sources: confirmed relations become real controllable sources.
//!
//! A group is admitted only from a confirmed, active catalog relation. Its own
//! MIoT instance is fetched and parsed exactly like an ordinary light; member
//! descriptions are never required. Group identity stays in memory for this
//! source slice (runtime publication will persist it later). Virtual groups
//! are cloud-only; physical groups keep their own DID's real route.

use super::{
    model::AdapterError,
    relations::ConfirmedGroup,
    state::{Context, DeviceRecord, GroupSource, Lifetime},
};
use crate::{
    device::{Availability, DeviceKey, PhysicalDevice},
    storage::CachedPhysicalDevice,
    xiaomi::{
        capability::parse_group,
        cloud::{CloudClient, CloudDeviceInfo},
    },
};
use futures_lite::future;
use std::{cell::Cell, cell::RefCell, rc::Rc};

impl Context {
    /// Admit or replace one confirmed group in memory. Unlike ordinary
    /// reconcile this never writes device identity to storage; only the
    /// confirmed relation and the native descriptor are authoritative here.
    pub(super) fn reconcile_group(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        let (candidate, admitted) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (slot.candidate.clone(), slot.admitted.clone())
        };
        let Some((cached, name)) = candidate.or_else(|| {
            admitted
                .as_ref()
                .map(|record| (record.cached.borrow().clone(), None))
        }) else {
            let reason = self.live.borrow().devices[did]
                .blocked
                .clone()
                .unwrap_or_else(|| "waiting for a native group description".into());
            self.problem(did, Some(reason));
            return Ok(());
        };
        let label = name
            .as_deref()
            .map(crate::storage::import_label)
            .unwrap_or_else(|| crate::runtime::default_device_label(&cached.model, did));
        match admitted {
            Some(record) => {
                if *record.cached.borrow() != cached {
                    // The replaced view must not stay Reachable holding stale
                    // light state. Runtime publication ownership is 07 work.
                    record.physical.set_availability(Availability::Unavailable(
                        "native group description replaced".into(),
                    ));
                    // Retire the old work's own subscriptions before the new
                    // view can receive messages for the same DID.
                    self.retire_device_notifications(did);
                    let (physical, receivers) =
                        PhysicalDevice::new(cached.physical_description(label));
                    self.registry.remove(record.physical.key());
                    self.registry.insert(physical.clone());
                    record.life.close();
                    {
                        let mut live = self.live.borrow_mut();
                        if let Some(work) = live.devices.get_mut(did).unwrap().work.take() {
                            work.life.close();
                        }
                        if let Some(session) = live.session.as_mut() {
                            session.devices.remove(did);
                        }
                    }
                    let replacement = Rc::new(DeviceRecord {
                        life: Lifetime::default(),
                        binding_generation: Cell::new(record.binding_generation.get() + 1),
                        cached: RefCell::new(cached),
                        physical,
                    });
                    self.live
                        .borrow_mut()
                        .devices
                        .get_mut(did)
                        .unwrap()
                        .admitted = Some(replacement.clone());
                    self.add_consumers(replacement, receivers);
                }
            }
            None => {
                let (physical, receivers) = PhysicalDevice::new(cached.physical_description(label));
                self.registry.insert(physical.clone());
                let record = Rc::new(DeviceRecord {
                    life: Lifetime::default(),
                    binding_generation: Cell::new(0),
                    cached: RefCell::new(cached),
                    physical,
                });
                self.live
                    .borrow_mut()
                    .devices
                    .get_mut(did)
                    .unwrap()
                    .admitted = Some(record.clone());
                self.add_consumers(record, receivers);
            }
        }
        self.live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .candidate = None;
        self.install_work(did);
        self.update_availability(did);
        let live = self.live.borrow();
        let slot = &live.devices[did];
        let diagnostic = slot.work.as_ref().and_then(|work| work.diagnostic());
        let problem = slot.blocked.clone().or(diagnostic);
        drop(live);
        self.problem(did, problem);
        if let Some(work) = self.live.borrow().devices[did].work.as_ref() {
            work.retry_subscriptions();
        }
        self.changed.notify(usize::MAX);
        Ok(())
    }

    /// Retire a group source without touching persisted ordinary identity.
    pub(super) fn remove_group(self: &Rc<Self>, did: &str) -> Result<(), AdapterError> {
        let key = DeviceKey::new("xiaomi", did);
        // Clear the retained physical view and retire the group's own native
        // subscriptions through the existing source mechanisms before the
        // slot disappears.
        if let Some(record) = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone())
        {
            record
                .physical
                .set_availability(Availability::Unavailable("native group withdrawn".into()));
        }
        self.retire_device_notifications(did);
        self.registry.remove(&key);
        {
            let mut live = self.live.borrow_mut();
            let Some(slot) = live.devices.remove(did) else {
                return Ok(());
            };
            if let Some(life) = slot.loading {
                life.close();
            }
            if let Some(record) = slot.admitted {
                record.life.close();
            }
            if let Some(work) = slot.work {
                work.life.close();
            }
            if let Some(session) = live.session.as_mut()
                && let Some(work) = session.devices.remove(did)
            {
                work.life.close();
            }
        }
        self.problem(did, None);
        self.commands.remove(did);
        self.changed.notify(usize::MAX);
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
    ) {
        let wanted = active
            .iter()
            .map(|group| group.membership.group_did.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let stale = self
            .live
            .borrow()
            .devices
            .iter()
            .filter(|(did, slot)| slot.group.is_some() && !wanted.contains(*did))
            .map(|(did, _)| did.clone())
            .collect::<Vec<_>>();
        for did in stale {
            if let Err(error) = self.remove_group(&did) {
                log::warn!("Retiring native group {did} failed: {error}");
            }
        }
        for group in active {
            let did = &group.membership.group_did;
            let needs_local = {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.entry(did.clone()).or_default();
                let known = slot.group.as_ref().map(|group| {
                    (
                        group.is_physical,
                        group.model.clone(),
                        group.instance_type.clone(),
                    )
                });
                slot.group = Some(GroupSource {
                    record_did: group.record_did.clone(),
                    model: group.model.clone(),
                    instance_type: group.instance_type.clone(),
                    members: group.membership.members.clone(),
                    is_physical: group.membership.is_physical,
                    online: group.online,
                });
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
                if let Err(error) = self.reconcile_group(did) {
                    log::warn!("Native group {did} reconcile failed: {error}");
                }
                continue;
            }
            let life = Rc::new(Lifetime::default());
            self.live.borrow_mut().devices.get_mut(did).unwrap().loading = Some(life.clone());
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
                let Some(result) = future::or(
                    async { Some(client.get_instance(&instance_type).await) },
                    async {
                        context.token_retired(&token).await;
                        None
                    },
                )
                .await
                else {
                    return Ok::<(), AdapterError>(());
                };
                let cleared = {
                    let mut live = context.live.borrow_mut();
                    let Some(slot) = live.devices.get_mut(&group_did) else {
                        return Ok::<(), AdapterError>(());
                    };
                    if slot
                        .loading
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &life))
                    {
                        slot.loading = None;
                        true
                    } else {
                        false
                    }
                };
                if !cleared {
                    return Ok::<(), AdapterError>(());
                }
                let current = context.current_token(&token)
                    && context.live.borrow().scope.as_ref().is_some_and(|scope| {
                        scope.uid == captured_scope.uid
                            && scope.home_id == captured_scope.home_id
                            && scope.group_id == captured_scope.group_id
                    })
                    && context
                        .live
                        .borrow()
                        .devices
                        .get(&group_did)
                        .is_some_and(|slot| {
                            slot.group.as_ref().is_some_and(|group| {
                                group.record_did == record_did
                                    && group.instance_type.as_deref()
                                        == Some(instance_type.as_str())
                                    && group.model == model
                            })
                        });
                if !current {
                    return Ok::<(), AdapterError>(());
                }
                match result {
                    Ok(instance) => {
                        match parse_group(
                            DeviceKey::new("xiaomi", group_did.clone()),
                            &model,
                            &instance,
                        ) {
                            Ok(device) => {
                                let cached = CachedPhysicalDevice {
                                    uid: scope.uid.clone(),
                                    home_id: scope.home_id.clone(),
                                    group_id: scope.group_id.clone(),
                                    model,
                                    instance_type,
                                    device,
                                };
                                if let Some(slot) =
                                    context.live.borrow_mut().devices.get_mut(&group_did)
                                {
                                    slot.blocked = None;
                                    slot.candidate = Some((cached, name));
                                }
                                context.reconcile_group(&group_did)?;
                            }
                            Err(error) => context.problem(&group_did, Some(error.to_string())),
                        }
                    }
                    Err(error) => {
                        context.problem(&group_did, Some(error.to_string()));
                        if error.is_unauthorized() {
                            context.cloud_error(&token, None, &error)?;
                        }
                    }
                }
                Ok::<(), AdapterError>(())
            });
            self.changed.notify(usize::MAX);
        }
    }
}
