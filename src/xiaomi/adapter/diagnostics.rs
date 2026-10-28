use super::{
    model::{
        AdapterStatus, DeviceStatus, FunctionStatus, GatewayStatus, ObservedPropertyStatus,
        PathKind, PendingDevice, RecoveryStatus, RoutingStatus, SubscriptionBinding,
    },
    routing::ControlPath,
    state::Context,
};
fn path_kind(path: ControlPath) -> PathKind {
    match path {
        ControlPath::Local(_) => PathKind::Local,
        ControlPath::Cloud(_) => PathKind::Cloud,
    }
}

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
            let read_only = self.is_read_only(&did);
            let mut usable = Vec::new();
            if !read_only {
                if self.local_candidate(&did, false).is_ok() {
                    usable.push(PathKind::Local);
                }
                if self.cloud_candidate(&did, true).is_ok() {
                    usable.push(PathKind::Cloud);
                }
            }
            let selected = self.control_path(&did).map(path_kind);
            let mut usable_read = Vec::new();
            if self.local_candidate(&did, false).is_ok() {
                usable_read.push(PathKind::Local);
            }
            if self.cloud_observation_candidate(&did, true).is_ok() {
                usable_read.push(PathKind::Cloud);
            }
            let selected_read = self.read_path(&did).map(path_kind);
            let physical_online =
                if self.has_sensors(&did) && super::routing::unreliable_cloud_online(&did) {
                    None
                } else {
                    let live = self.live.borrow();
                    let slot = live.devices.get(&did).unwrap();
                    slot.cloud_access
                        .as_ref()
                        .map(|access| access.online)
                        .or_else(|| {
                            slot.access
                                .as_ref()
                                .and_then(|access| access.as_ref().ok())
                                .map(|access| access.online)
                        })
                };
            let notification = self.active_notification(&did);
            let old = previous.devices.iter().find(|device| device.did == did);
            let switched = old.is_some_and(|old| {
                old.selected_control != selected
                    || old.selected_read != selected_read
                    || old.notification_source != notification
            });
            let last_switch_reason = {
                let mut live = self.live.borrow_mut();
                let slot = live.devices.get_mut(&did).unwrap();
                if switched {
                    slot.last_switch_reason = Some(reason.into());
                }
                slot.last_switch_reason.clone()
            };
            let unavailable_reason = (!self.device_path_available(&did)).then(|| {
                format!(
                    "local: {}; cloud: {}",
                    self.local_candidate(&did, false)
                        .err()
                        .expect("no local device path"),
                    self.cloud_observation_candidate(&did, true)
                        .expect_err("no cloud device path")
                )
            });
            let sync_limited_reason = notification
                .is_none()
                .then(|| self.notification_reason(&did));
            let work = self.live.borrow().devices[&did]
                .admitted
                .as_ref()
                .unwrap()
                .clone();
            let functions = work
                .native
                .device()
                .functions()
                .iter()
                .map(|function| {
                    let notification_source = work
                        .function_observer(function.id())
                        .filter(|source| self.source_valid_for(&did, function.id(), source))
                        .map(|source| match source {
                            super::notifications::NotificationSource::Local(_) => PathKind::Local,
                            super::notifications::NotificationSource::Cloud(_) => PathKind::Cloud,
                        });
                    FunctionStatus {
                        function: function.id().to_owned(),
                        selected_read: self.read_path_for(&did, function.id()).map(path_kind),
                        notification_source,
                        sync_limited_reason: notification_source.is_none().then(|| {
                            work.function_subscription_problem(function.id())
                                .unwrap_or_else(|| self.notification_reason(&did))
                        }),
                        read_problem: work.refresh.borrow().problem_for(function.id()),
                        observation_source: work.observation_source(function.id()),
                        subscriptions: if function.state().is_some() {
                            work.subscription_status(function.id())
                        } else {
                            Vec::new()
                        },
                        properties: function.curtain().map_or_else(Vec::new, |curtain| {
                            let subscriptions = work.subscription_status(function.id());
                            [
                                ("Current position", curtain.current().property()),
                                ("Target position", curtain.target().property()),
                                ("Status", curtain.status().property()),
                            ]
                            .into_iter()
                            .map(|(name, binding)| ObservedPropertyStatus {
                                name,
                                binding,
                                observation_source: work.property_source(function.id(), binding),
                                read_problem: work
                                    .refresh
                                    .borrow()
                                    .property_problem(function.id(), binding),
                                subscriptions: subscriptions
                                    .iter()
                                    .filter(|item| {
                                        item.binding == SubscriptionBinding::Property(binding)
                                    })
                                    .cloned()
                                    .collect(),
                            })
                            .collect()
                        }),
                    }
                })
                .collect();
            let battery_problem = work.battery_problem();
            let channels = work
                .native
                .device()
                .functions()
                .iter()
                .filter(|function| function.kind().is_control())
                .map(|function| function.id().to_owned())
                .collect::<Vec<_>>();
            let mut commands = self.commands.snapshot(&did, channels.into_iter());
            for command in &mut commands {
                command.light_actions = work
                    .physical()
                    .function(&command.channel)
                    .unwrap()
                    .light_action_diagnostics();
            }
            devices.push(DeviceStatus {
                did,
                read_only,
                usable_control_paths: usable,
                selected_control: selected,
                usable_read_paths: usable_read,
                selected_read,
                physical_online,
                notification_source: notification,
                unavailable_reason,
                sync_limited_reason,
                battery_problem,
                last_switch_reason,
                commands,
                functions,
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
        let work = self.live.borrow().devices[did].admitted.clone();
        if let Some(work) = work {
            if work.notifications_preparing() {
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
