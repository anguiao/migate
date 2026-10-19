use super::{
    model::{
        AdapterStatus, DeviceStatus, GatewayStatus, PathKind, PendingDevice, RecoveryStatus,
        RoutingStatus,
    },
    routing::ControlPath,
    state::Context,
};
impl Context {
    pub(super) fn diagnostics(&self) -> AdapterStatus {
        self.update_routing("access conditions changed");
        self.status.borrow().clone()
    }
    pub(super) fn update_routing(&self, reason: &str) {
        // Eligibility can change at a deadline before its maintenance task is polled.
        let reason = if reason == "access conditions changed" {
            let expired = self
                .live
                .borrow()
                .credentials
                .as_ref()
                .is_some_and(|record| record.tokens.expires_at <= self.clock.unix_now());
            if expired {
                "access token expired"
            } else if self.live.borrow().session.is_some()
                && self
                    .status
                    .borrow()
                    .authentication
                    .certificate
                    .is_some_and(|validity| validity.not_after <= self.clock.unix_now())
            {
                "local gateway certificate expired"
            } else {
                reason
            }
        } else {
            reason
        };
        let previous = self.status.borrow().clone();
        let session = self
            .live
            .borrow()
            .session
            .as_ref()
            .map(|s| s.access.clone());
        let mut routing = RoutingStatus {
            priority: self.priority(),
            local_connected: session.is_some(),
            business_degraded: session.as_ref().is_some_and(|a| a.is_degraded()),
            recovery: session.as_ref().map_or(RecoveryStatus::None, |a| {
                a.recovery_status(self.clock.as_ref())
            }),
            last_switch_reason: previous.routing.last_switch_reason.clone(),
        };
        if routing.priority != previous.routing.priority {
            routing.last_switch_reason = Some(reason.into());
        }
        let dids = self
            .live
            .borrow()
            .devices
            .iter()
            .filter_map(|(did, slot)| slot.admitted.as_ref().map(|_| did.clone()))
            .collect::<Vec<_>>();
        let mut devices = Vec::new();
        for did in dids {
            let mut usable = Vec::new();
            if self.local_candidate(&did, false).is_ok() {
                usable.push(PathKind::Local);
            }
            if self.cloud_candidate(&did, true).is_ok() {
                usable.push(PathKind::Cloud);
            }
            let selected = self.control_path(&did).map(|path| match path {
                ControlPath::Local(_) => PathKind::Local,
                ControlPath::Cloud(_) => PathKind::Cloud,
            });
            let notification = self.active_notification(&did);
            let old = previous.devices.iter().find(|device| device.did == did);
            let switched = old.is_some_and(|old| {
                old.selected_control != selected || old.notification_source != notification
            });
            let last_switch_reason = {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.get_mut(&did).unwrap();
                if switched {
                    slot.last_switch_reason = Some(reason.into());
                }
                slot.last_switch_reason.clone()
            };
            let unavailable_reason = selected.is_none().then(|| {
                format!(
                    "local: {}; cloud: {}",
                    self.local_candidate(&did, false)
                        .err()
                        .expect("no local control path"),
                    self.cloud_candidate(&did, true)
                        .expect_err("no cloud control path")
                )
            });
            let sync_limited_reason = notification
                .is_none()
                .then(|| self.notification_reason(&did));
            let channels = self.live.borrow().devices[&did]
                .admitted
                .as_ref()
                .unwrap()
                .cached
                .borrow()
                .device
                .functions
                .iter()
                .map(|function| function.description.id.clone())
                .collect::<Vec<_>>();
            let mut commands = self.commands.snapshot(&did, channels.into_iter());
            let record = self.live.borrow().devices[&did]
                .admitted
                .as_ref()
                .unwrap()
                .clone();
            for command in &mut commands {
                command.light_actions = record
                    .physical
                    .function(&command.channel)
                    .unwrap()
                    .light_action_diagnostics();
            }
            devices.push(DeviceStatus {
                did,
                usable_control_paths: usable,
                selected_control: selected,
                notification_source: notification,
                unavailable_reason,
                sync_limited_reason,
                last_switch_reason,
                commands,
            });
        }
        let mut current = self.status.borrow_mut();
        if current.routing != routing || current.devices != devices {
            current.routing = routing;
            current.devices = devices;
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn note_notification_switch(&self, did: &str, reason: &str) {
        self.live
            .borrow_mut()
            .devices
            .get_mut(did)
            .unwrap()
            .last_switch_reason = Some(reason.into());
        self.update_routing(reason);
    }
    fn notification_reason(&self, did: &str) -> String {
        if self.is_virtual_group(did) {
            return "member feedback unavailable or subscriptions not ready".into();
        }
        let work = self.live.borrow().devices[did].work.clone();
        if let Some(work) = work {
            if work.subscribe_running.get() || work.cloud_running.get() {
                return "notification subscriptions preparing".into();
            }
            if let Some(reason) = work.subscription_problem() {
                return reason;
            }
        }
        let local = match self.local_candidate(did, true) {
            Ok(_) => "notification subscriptions not ready".into(),
            Err(reason) => reason,
        };
        let cloud = match self.cloud_candidate(did, false) {
            Ok(()) => "notifications disconnected or subscriptions not ready".into(),
            Err(reason) => reason,
        };
        format!("local notifications: {local}; cloud notifications: {cloud}")
    }
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
