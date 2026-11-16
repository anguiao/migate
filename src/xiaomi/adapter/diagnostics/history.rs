//! Routing history stores selections, never diagnostic projections.

use super::super::{
    routing::{PathPriority, Summaries},
    state::Context,
};
use crate::device::{DeviceKey, FunctionKey};
use std::collections::BTreeSet;

pub(in crate::xiaomi::adapter) struct PriorityHistory {
    priority: PathPriority,
    pub(in crate::xiaomi::adapter) reason: Option<String>,
}

impl Default for PriorityHistory {
    fn default() -> Self {
        Self {
            priority: PathPriority::LocalFirst,
            reason: None,
        }
    }
}

impl Context {
    /// Global access changes may affect every serving source.
    pub(in crate::xiaomi::adapter) fn update_routing(&self, reason: &str) {
        let evaluated = self.routing_summaries_at(self.clock.unix_now(), None);
        {
            let mut history = self.routing_history.borrow_mut();
            if history.priority != evaluated.priority {
                history.priority = evaluated.priority;
                history.reason = Some(reason.into());
            }
        }
        self.record_selections(&evaluated, reason, &BTreeSet::new());
        self.status_changed.notify(usize::MAX);
    }

    pub(in crate::xiaomi::adapter) fn update_device_routing(&self, did: &str, reason: &str) {
        self.update_routing_for([did], reason, None);
    }

    pub(in crate::xiaomi::adapter) fn note_notification_switch(
        &self,
        did: &str,
        id: &str,
        reason: &str,
    ) {
        let function = FunctionKey::new(DeviceKey::new("xiaomi", did), id);
        self.update_routing_for([did], reason, Some(&function));
    }

    /// Device events compare all dependent summaries. An actual handover is
    /// forced only for its source DID and groups using that exact function.
    pub(in crate::xiaomi::adapter) fn update_routing_for<'a>(
        &self,
        dids: impl IntoIterator<Item = &'a str>,
        reason: &str,
        handover: Option<&FunctionKey>,
    ) {
        let mut affected = dids.into_iter().map(str::to_owned).collect::<BTreeSet<_>>();
        let graph = self.graph.borrow().clone();
        let mut forced = BTreeSet::new();
        if let Some(function) = handover {
            forced.insert(function.device.id.clone());
            if let Some(groups) = graph.memberships.get(function) {
                forced.extend(
                    groups
                        .iter()
                        .filter(|did| graph.rules.get(*did).is_some_and(|rule| !rule.is_physical))
                        .cloned(),
                );
            }
        }
        let mut pending = affected.iter().cloned().collect::<Vec<_>>();
        while let Some(did) = pending.pop() {
            let key = DeviceKey::new("xiaomi", &did);
            let first = FunctionKey::new(key.clone(), "");
            for (_, groups) in graph
                .memberships
                .range(first..)
                .take_while(|(member, _)| member.device == key)
            {
                for group in groups {
                    if graph.rules.get(group).is_some_and(|rule| !rule.is_physical)
                        && affected.insert(group.clone())
                    {
                        pending.push(group.clone());
                    }
                }
            }
        }
        let evaluated = self.routing_summaries_at(self.clock.unix_now(), Some(&affected));
        self.record_selections(&evaluated, reason, &forced);
        self.status_changed.notify(usize::MAX);
    }

    fn record_selections(&self, evaluated: &Summaries, reason: &str, forced: &BTreeSet<String>) {
        let mut directory = self.directory.borrow_mut();
        for (did, summary) in &evaluated.devices {
            let slot = directory.devices.get_mut(did).unwrap();
            if slot
                .routing_selection
                .is_some_and(|old| old != summary.selection)
                || forced.contains(did)
            {
                slot.last_switch_reason = Some(
                    summary
                        .deadline
                        .map(|deadline| deadline.reason())
                        .unwrap_or(reason)
                        .into(),
                );
            }
            // An entirely unavailable first admission also establishes a baseline.
            slot.routing_selection = Some(summary.selection);
        }
    }
}
