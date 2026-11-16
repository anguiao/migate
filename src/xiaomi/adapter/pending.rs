//! Read-only projection of independently owned device preparation problems.

use super::{model::PendingDevice, state::Context};
use std::collections::BTreeSet;

impl Context {
    pub(super) fn pending_devices(&self) -> Vec<PendingDevice> {
        let directory = self.directory.borrow();
        let hub = self.hub.borrow();
        let dids = directory
            .devices
            .keys()
            .chain(directory.planning_problems().keys())
            .collect::<BTreeSet<_>>();
        dids.into_iter()
            .filter_map(|did| {
                let slot = directory.devices.get(did);
                let planning = directory.planning_problems().get(did);
                let mut problems = slot
                    .and_then(|slot| slot.blocked.as_ref())
                    .into_iter()
                    .chain(planning.map(|problem| &problem.reason))
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(work) = slot.and_then(|slot| slot.admitted.as_ref())
                    && !work.life.is_closed()
                {
                    problems.extend(work.pending_problems());
                }
                if problems.is_empty() {
                    return None;
                }
                let name = slot
                    .and_then(|slot| {
                        hub.access(did)
                            .and_then(|access| access.as_ref().ok())
                            .and_then(|access| access.name.as_ref())
                            .or_else(|| {
                                slot.candidate
                                    .as_ref()
                                    .and_then(|source| source.raw.name.as_ref())
                            })
                            .or_else(|| {
                                slot.committed
                                    .as_ref()
                                    .and_then(|source| source.raw.name.as_ref())
                            })
                            .or(slot.catalog_name.as_ref())
                    })
                    .or_else(|| planning.and_then(|problem| problem.committed_name.as_ref()))
                    .cloned()
                    .unwrap_or_else(|| did.clone());
                Some(PendingDevice {
                    did: did.clone(),
                    name,
                    reason: problems.join("; "),
                })
            })
            .collect()
    }
}
