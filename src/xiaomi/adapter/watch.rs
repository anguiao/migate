//! Raw member property observation for virtual native groups.
//!
//! Members of virtual groups are watched by DID only: the official cloud topic
//! `device/{did}/up/properties_changed/#` delivers valid property events even
//! when MiGate has no parsed description or independent endpoint for the
//! member. Such events never become member state; they only mark the native
//! group dirty so the group's own properties are read again.

use super::{
    model::AdapterError,
    state::{Context, Lifetime},
};
use crate::{
    device::DeviceKey,
    xiaomi::{auth::FailureReason, cloud::NotificationError, cloud::NotificationHandle},
};
use std::{
    collections::btree_map::Entry,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

/// Raw member observers keyed by member DID. A member may belong to several
/// virtual groups; the group's own read is coalesced by its device work.
#[derive(Default)]
pub(super) struct Watches {
    pub(super) members: BTreeMap<String, MemberWatch>,
}
#[derive(Default)]
pub(super) struct MemberWatch {
    pub(super) groups: BTreeSet<DeviceKey>,
    pub(super) state: WatchState,
    /// Owns the current preparation; closed on removal or restart so stale
    /// callbacks can never mark a newer preparation Ready.
    pub(super) lifetime: Rc<Lifetime>,
}
#[derive(Default, PartialEq)]
pub(super) enum WatchState {
    #[default]
    Pending,
    Preparing,
    Ready,
    Failed(String),
}
impl Context {
    pub(super) fn member_property_event(
        &self,
        handle: &NotificationHandle,
        did: &str,
        _siid: u32,
        _piid: u32,
        _value: &serde_json::Value,
        retained: bool,
    ) {
        if !self.current_notifications(handle) {
            return;
        }
        // Retained snapshots are dirty-only and must never become
        // external-operation evidence; the origin stays available to callers.
        let _ = retained;
        self.notify_member_watch(did);
    }
    /// A valid member event only schedules a read of the group's own
    /// properties. Member values are never interpreted as group state.
    pub(super) fn group_member_changed(&self, group: &DeviceKey) {
        let work = self
            .live
            .borrow()
            .devices
            .get(&group.id)
            .and_then(|slot| slot.work.clone());
        if let Some(work) = work {
            work.request_member_read();
        }
    }
    /// Rebuild raw member observers from the confirmed virtual relations and
    /// keep the cloud notification session aligned with them. Each DID has one
    /// shared watcher; preparation is owned and awaited through the session's
    /// task set, and a failure stays visible instead of silently dropping.
    pub(super) fn sync_member_watches(self: &Rc<Self>, restart: bool) {
        let desired = self.relations.borrow().member_watches();
        let mut changed_groups = BTreeSet::new();
        let mut removed = Vec::new();
        {
            let mut watches = self.watches.borrow_mut();
            let current = watches.members.keys().cloned().collect::<Vec<_>>();
            for did in current {
                let Some(groups) = desired.get(&did) else {
                    if let Some(watch) = watches.members.remove(&did) {
                        changed_groups.extend(watch.groups.iter().cloned());
                        watch.lifetime.close();
                        removed.push(did);
                    }
                    continue;
                };
                let watch = watches.members.get_mut(&did).unwrap();
                if watch.groups != *groups {
                    changed_groups.extend(watch.groups.symmetric_difference(groups).cloned());
                    watch.groups = groups.clone();
                }
            }
            for (did, groups) in &desired {
                if let Entry::Vacant(entry) = watches.members.entry(did.clone()) {
                    // A group gaining a member refreshes actively; a brand-new
                    // group without work is covered by its initial read.
                    changed_groups.extend(groups.iter().cloned());
                    entry.insert(MemberWatch {
                        groups: groups.clone(),
                        state: WatchState::Pending,
                        lifetime: Rc::new(Lifetime::default()),
                    });
                }
            }
            if restart {
                for watch in watches.members.values_mut() {
                    watch.lifetime.close();
                    watch.state = WatchState::Pending;
                }
            }
        }
        // Changed watch associations recover the affected groups even when the
        // member already has a Ready shared raw subscription.
        for group in changed_groups {
            self.group_member_changed(&group);
        }
        let session = self.notifications.borrow().as_ref().cloned();
        let Some(session) = session else {
            return;
        };
        for did in &removed {
            drop(session.handle.unsubscribe_raw_properties(did));
        }
        let pending = {
            let mut watches = self.watches.borrow_mut();
            let mut pending = Vec::new();
            for (did, watch) in watches.members.iter_mut() {
                if matches!(watch.state, WatchState::Ready | WatchState::Preparing) && !restart {
                    continue;
                }
                watch.lifetime.close();
                let life = Rc::new(Lifetime::default());
                watch.lifetime = life.clone();
                watch.state = WatchState::Preparing;
                pending.push((did.clone(), life));
            }
            pending
        };
        for (did, life) in pending {
            let context = self.clone();
            let handle = session.handle.clone();
            let subscriptions = session.subscriptions.clone();
            let token = session.token().to_string();
            session.tasks.spawn(async move {
                let current = || !life.is_closed() && context.current_notifications(&handle);
                let watched_groups = || {
                    context
                        .watches
                        .borrow()
                        .members
                        .get(&did)
                        .map(|watch| watch.groups.clone())
                        .unwrap_or_default()
                };
                let mut failure = None;
                for attempt in 0..4 {
                    let Some(permit) = subscriptions
                        .acquire(&context.changed, &life, &current)
                        .await
                    else {
                        return Ok::<(), AdapterError>(());
                    };
                    if !current() {
                        return Ok::<(), AdapterError>(());
                    }
                    let result = handle.subscribe_raw_properties(&did).await;
                    drop(permit);
                    if !current() {
                        return Ok::<(), AdapterError>(());
                    }
                    match result {
                        Ok(()) => {
                            if let Some(watch) = context.watches.borrow_mut().members.get_mut(&did)
                            {
                                watch.state = WatchState::Ready;
                            }
                            for group in watched_groups() {
                                if let Some(work) = context
                                    .live
                                    .borrow()
                                    .devices
                                    .get(&group.id)
                                    .and_then(|slot| slot.work.clone())
                                {
                                    work.request_member_read();
                                    work.report(&context, &group.id);
                                }
                            }
                            context.changed.notify(usize::MAX);
                            return Ok::<(), AdapterError>(());
                        }
                        Err(NotificationError::Unauthorized) => {
                            context.token_rejected(
                                &token,
                                FailureReason::CloudNotification(NotificationError::Unauthorized),
                            )?;
                            failure = Some(NotificationError::Unauthorized);
                            break;
                        }
                        Err(error) => {
                            failure = Some(error);
                            if attempt < 3 {
                                context
                                    .clock
                                    .wait_until(
                                        context.clock.now() + std::time::Duration::from_secs(1),
                                    )
                                    .await;
                                if !current() {
                                    return Ok::<(), AdapterError>(());
                                }
                            }
                        }
                    }
                }
                if let Some(error) = failure {
                    let message =
                        format!("raw member watch {did} exhausted after 4 attempts: {error}");
                    log::warn!("{message}");
                    if let Some(watch) = context.watches.borrow_mut().members.get_mut(&did) {
                        watch.state = WatchState::Failed(message.clone());
                    }
                    for group in watched_groups() {
                        if let Some(work) = context
                            .live
                            .borrow()
                            .devices
                            .get(&group.id)
                            .and_then(|slot| slot.work.clone())
                        {
                            work.report(&context, &group.id);
                        }
                    }
                    context.changed.notify(usize::MAX);
                }
                Ok::<(), AdapterError>(())
            });
        }
    }

    /// Aggregate failures from the actual per-DID watch states for one group.
    pub(super) fn watch_problem(&self, group_did: &str) -> Option<String> {
        let wanted = DeviceKey::new("xiaomi", group_did);
        let values = self
            .watches
            .borrow()
            .members
            .iter()
            .filter(|(_, watch)| watch.groups.contains(&wanted))
            .filter_map(|(did, watch)| match &watch.state {
                WatchState::Failed(reason) => Some(reason.clone()),
                WatchState::Pending | WatchState::Preparing => {
                    Some(format!("raw member watch {did} preparing"))
                }
                WatchState::Ready => None,
            })
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }

    /// A valid property event for a DID also refreshes every virtual group
    /// that observes it, independently of whether the DID has its own work.
    pub(super) fn notify_member_watch(&self, did: &str) {
        let groups = self
            .watches
            .borrow()
            .members
            .get(did)
            .map(|watch| watch.groups.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for group in groups {
            self.group_member_changed(&group);
        }
    }
}
