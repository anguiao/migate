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
                let group = directory.group(did);
                let mut problems =
                    group
                        .filter(|group| group.instance_type.is_none())
                        .map(|_| "native group description has no instance type".to_owned())
                        .into_iter()
                        .chain(slot.into_iter().flat_map(|slot| {
                            slot.description_problems.iter().map(ToString::to_string)
                        }))
                        .chain(planning.map(|problem| problem.reason.clone()))
                        .collect::<Vec<_>>();
                if let Some(work) = slot.and_then(|slot| slot.admitted.as_ref())
                    && !work.life.is_closed()
                {
                    problems.extend(work.pending_problems());
                }
                if problems.is_empty() {
                    return None;
                }
                let name = group
                    .and_then(|group| group.name.as_ref())
                    .or_else(|| slot.and_then(|slot| slot.catalog_name.as_ref()))
                    .or_else(|| {
                        hub.access(did)
                            .and_then(|access| access.as_ref().ok())
                            .and_then(|access| access.name.as_ref())
                    })
                    .or_else(|| {
                        slot.and_then(|slot| slot.latest_description.as_ref())
                            .and_then(|source| source.raw.name.as_ref())
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
