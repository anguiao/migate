use super::{
    model::{GatewayStatus, PendingDevice},
    state::Context,
};
impl Context {
    pub(super) fn request_sync(&self) {
        self.sync_requested.set(true);
        self.sync_changed.notify(usize::MAX);
    }
    pub(super) fn set_gateway(&self, status: GatewayStatus) {
        let mut current = self.status.borrow_mut();
        if current.gateway != status {
            log::info!("Xiaomi gateway state: {status:?}");
            current.gateway = status;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn set_catalog_problem(&self, problem: Option<String>) {
        *self.cloud_problem.borrow_mut() = problem;
        self.update_catalog_problem();
    }
    pub(super) fn set_access_problem(&self, problem: Option<String>) {
        *self.access_problem.borrow_mut() = problem;
        self.update_catalog_problem();
    }
    fn update_catalog_problem(&self) {
        let problems = [
            self.cloud_problem.borrow().clone(),
            self.access_problem.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let problem = (!problems.is_empty()).then(|| problems.join("; "));
        let mut current = self.status.borrow_mut();
        if current.catalog_problem != problem {
            current.catalog_problem = problem;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn problem(&self, did: &str, reason: Option<String>) {
        let name = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.access.as_ref())
            .and_then(|access| access.as_ref().ok())
            .and_then(|access| access.name.clone())
            .unwrap_or_else(|| did.into());
        let mut status = self.status.borrow_mut();
        status.pending.retain(|entry| entry.did != did);
        if let Some(reason) = reason {
            status.pending.push(PendingDevice {
                did: did.into(),
                name,
                reason,
            });
            status.pending.sort_by(|a, b| a.did.cmp(&b.did));
        }
        self.status_changed.notify(usize::MAX);
    }
}
