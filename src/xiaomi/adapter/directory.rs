//! Latest home facts and descriptions, kept apart from installed configuration.

use super::{
    relations::{ConfirmedGroup, RelationUpdate, Relations},
    state::{Backoff, Lifetime},
    work::DeviceWork,
};
use crate::{
    configuration::{Scope, SplitDevice},
    xiaomi::{
        capability::SourceDescription,
        cloud::CloudError,
        mijia::{CloudDeviceCatalog, CloudDeviceInfo},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Duration,
};

pub(super) struct Directory {
    pub(super) scope: Option<Scope>,
    pub(super) devices: BTreeMap<String, DeviceSlot>,
    relations: Relations,
    split_devices: Vec<SplitDevice>,
    scope_valid: bool,
    problem: Option<String>,
    sync_not_before: Option<Duration>,
    sync_backoff: Backoff,
}

pub(super) struct HomeUpdate {
    pub(super) removed: Vec<String>,
    pub(super) membership_changed: bool,
}

impl Directory {
    pub(super) fn restored(
        scope: Option<Scope>,
        relations: Relations,
        split_devices: Vec<SplitDevice>,
    ) -> Self {
        Self {
            scope,
            devices: BTreeMap::new(),
            relations,
            split_devices,
            scope_valid: false,
            problem: None,
            sync_not_before: None,
            sync_backoff: Backoff::new(1, 30),
        }
    }

    /// Establish the latest complete home before retiring its withdrawn sources.
    pub(super) fn select_home(
        &mut self,
        scope: Scope,
        split_devices: Vec<SplitDevice>,
        members: &BTreeSet<String>,
    ) -> HomeUpdate {
        let scope_changed = self.scope.as_ref().is_some_and(|old| old != &scope);
        let membership_changed = self
            .devices
            .iter()
            .filter(|(_, slot)| !slot.group)
            .map(|(did, _)| did)
            .ne(members.iter());
        let removed = self
            .devices
            .iter()
            .filter(|(did, slot)| scope_changed || (!slot.group && !members.contains(*did)))
            .map(|(did, _)| did.clone())
            .collect();
        self.scope = Some(scope);
        self.split_devices = split_devices;
        HomeUpdate {
            removed,
            membership_changed,
        }
    }

    pub(super) fn confirm_members(
        &mut self,
        members: &BTreeSet<String>,
        mut infos: BTreeMap<String, CloudDeviceInfo>,
    ) {
        for did in members {
            self.devices.entry(did.clone()).or_default();
        }
        for (did, slot) in &mut self.devices {
            if !slot.group {
                slot.confirmed = true;
                slot.cloud_access = infos.remove(did);
                slot.cloud_forbidden = false;
            }
        }
        self.confirm_scope();
    }

    pub(super) fn invalidate_scope(&mut self) {
        self.scope_valid = false;
    }

    pub(super) fn confirm_scope(&mut self) {
        self.scope_valid = true;
    }

    pub(super) fn scope_valid(&self) -> bool {
        self.scope_valid
    }

    pub(super) fn split_devices(&self) -> &[SplitDevice] {
        &self.split_devices
    }

    pub(super) fn confirmed_groups(&self) -> Vec<ConfirmedGroup> {
        self.relations.snapshot()
    }

    pub(super) fn confirm_catalog(
        &mut self,
        catalog: &CloudDeviceCatalog,
        scope: &Scope,
        members: &BTreeSet<String>,
    ) -> RelationUpdate {
        self.relations.apply(catalog, scope, members)
    }

    pub(super) fn matches_group_fetch(
        &self,
        group_did: &str,
        record_did: &str,
        model: &str,
        instance_type: &str,
    ) -> bool {
        self.relations
            .matches_fetch(group_did, record_did, model, instance_type)
    }

    /// Confirm access metadata without replacing the committed group graph.
    pub(super) fn confirm_group(&mut self, group: &ConfirmedGroup) -> (bool, Option<Rc<Lifetime>>) {
        let did = &group.membership.group_did;
        let slot = self.devices.entry(did.clone()).or_default();
        let recovered_target =
            slot.group && slot.description_target_missing && group.instance_type.is_some();
        slot.confirmed = true;
        // A retained relation with a read error cannot restore revoked access.
        if group.error.is_none() {
            slot.cloud_forbidden = false;
        }
        slot.group = true;
        slot.description_target_missing = group.instance_type.is_none();
        slot.cloud_access = group
            .instance_type
            .as_ref()
            .map(|instance_type| CloudDeviceInfo {
                did: did.clone(),
                model: group.model.clone(),
                instance_type: instance_type.clone(),
                online: group.online,
                lan: crate::xiaomi::mijia::LanCredentials::NotIp,
            });
        let target = group
            .instance_type
            .as_ref()
            .map(|urn| (group.model.clone(), urn.clone()));
        (recovered_target, slot.retarget_loading(&target))
    }

    pub(super) fn accept_description(&mut self, did: &str, description: SourceDescription) {
        if let Some(slot) = self.devices.get_mut(did) {
            slot.candidate = Some(description);
            slot.blocked = None;
            slot.recompute_failed = false;
        }
    }

    pub(super) fn device_error(&mut self, did: &str, error: &CloudError) {
        if let Some(slot) = self.devices.get_mut(did) {
            if error.is_device_offline()
                && let Some(access) = &mut slot.cloud_access
            {
                access.online = false;
            }
            if error.is_device_forbidden() {
                slot.cloud_forbidden = true;
            }
        }
    }

    pub(super) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    pub(super) fn catalog_result(&mut self, problem: Option<String>) -> bool {
        if self.problem == problem {
            return false;
        }
        self.problem = problem;
        true
    }

    pub(super) fn sync_deadline(&self) -> Option<Duration> {
        self.sync_not_before
    }

    pub(super) fn begin_sync(&mut self) {
        self.sync_not_before = None;
    }

    pub(super) fn sync_failed(&mut self, now: Duration) -> Duration {
        let due = now + self.sync_backoff.failed();
        self.sync_not_before = Some(due);
        due
    }

    pub(super) fn retry_deadline(&mut self, now: Duration) -> Duration {
        self.sync_not_before
            .unwrap_or_else(|| self.sync_failed(now))
    }

    pub(super) fn reset_sync(&mut self) {
        self.sync_not_before = None;
        self.sync_backoff = Backoff::new(1, 30);
    }
}

/// One entry owns all preparation and admission state for a current home member.
#[derive(Default)]
pub(super) struct DeviceSlot {
    /// This slot is a native group source. Executable group rules live only in
    /// the installed committed graph.
    pub(super) group: bool,
    pub(super) lan: super::lan::LanSlot,
    /// Latest complete source fact, retained while its commit is pending.
    pub(super) candidate: Option<SourceDescription>,
    /// Source fact paired with the installed configuration.
    pub(super) committed: Option<SourceDescription>,
    /// Startup recognition failed; unchanged directory facts cannot clear its
    /// diagnostic. Only a newly recognized description resolves it.
    pub(super) recompute_failed: bool,
    /// A confirmed group record lacked the target instance type.
    pub(super) description_target_missing: bool,
    pub(super) confirmed: bool,
    /// The installed native runtime; native control and subscriptions live here.
    pub(super) admitted: Option<Rc<DeviceWork>>,
    pub(super) cloud_access: Option<CloudDeviceInfo>,
    pub(super) cloud_forbidden: bool,
    /// The one in-flight directory load and the model/URN target it may
    /// apply. `None` target is a genuine first read without metadata facts.
    pub(super) loading: Option<Loading>,
    pub(super) blocked: Option<String>,
    pub(super) last_switch_reason: Option<String>,
}
#[derive(Clone)]
pub(super) struct Loading {
    pub(super) life: Rc<Lifetime>,
    pub(super) target: Option<(String, String)>,
}
impl DeviceSlot {
    /// Retire a load aimed at an older target; online changes keep it.
    pub(super) fn retarget_loading(
        &mut self,
        target: &Option<(String, String)>,
    ) -> Option<Rc<Lifetime>> {
        if self
            .loading
            .as_ref()
            .is_some_and(|loading| &loading.target != target)
        {
            self.loading.take().map(|loading| loading.life)
        } else {
            None
        }
    }
    pub(super) fn start_loading(&mut self, target: Option<(String, String)>) -> Rc<Lifetime> {
        debug_assert!(self.loading.is_none());
        let life = Rc::new(Lifetime::default());
        self.loading = Some(Loading {
            life: life.clone(),
            target,
        });
        life
    }
    pub(super) fn loading_is(&self, life: &Rc<Lifetime>) -> bool {
        self.loading
            .as_ref()
            .is_some_and(|loading| Rc::ptr_eq(&loading.life, life))
    }
    /// Finish this exact load once; a superseded load is left untouched.
    pub(super) fn finish_loading(&mut self, life: &Rc<Lifetime>) -> bool {
        if self.loading_is(life) {
            self.loading = None;
            true
        } else {
            false
        }
    }
    /// A directory description read is needed only when the latest valid
    /// fact is missing or its metadata differs from the current access facts.
    pub(super) fn needs_description(&self) -> bool {
        if self.loading.is_some() {
            return false;
        }
        let Some(source) = &self.candidate else {
            return true;
        };
        self.cloud_access.as_ref().is_some_and(|access| {
            access.model != source.raw.model || access.instance_type != source.raw.instance_type
        })
    }
}
