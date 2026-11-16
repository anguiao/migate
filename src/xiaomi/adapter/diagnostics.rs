use super::{
    model::{
        AdapterStatus, DeviceStatus, DiagnosticObservation, FunctionStatus, GatewayStatus,
        LowSpeedStatus, ObservedPropertyStatus, PathKind, PendingDevice, RoutingStatus,
        SharedFanSpeedStatus, SharedFeedbackStatus, SubscriptionBinding,
    },
    routing::ControlPath,
    state::Context,
    work::DeviceWork,
};
use crate::xiaomi::capability::PropertyBinding;

fn path_kind(path: ControlPath) -> PathKind {
    match path {
        ControlPath::Local(_) => PathKind::Local,
        ControlPath::Direct(_) => PathKind::Direct,
        ControlPath::Cloud(_) => PathKind::Cloud,
    }
}

impl Context {
    pub(super) fn diagnostics(&self) -> AdapterStatus {
        self.update_routing("access conditions changed");
        let cached = self.status.borrow();
        AdapterStatus {
            auth: self.auth_report(),
            gateway: self.hub.borrow().gateway().clone(),
            catalog_problem: self.catalog_problem(),
            pending: cached.pending.clone(),
            routing: cached.routing.clone(),
            devices: cached.devices.clone(),
        }
    }
    pub(super) fn update_routing(&self, reason: &str) {
        // Eligibility can change at a deadline before its maintenance task is polled.
        let reason = if reason == "access conditions changed" {
            let expired = self
                .credentials
                .borrow()
                .token_expired(self.clock.unix_now());
            if expired {
                "access token expired"
            } else if self.hub.borrow().session().is_some()
                && self
                    .credentials
                    .borrow()
                    .certificate_validity()
                    .is_some_and(|validity| validity.not_after <= self.clock.unix_now())
            {
                "hub certificate expired"
            } else {
                reason
            }
        } else {
            reason
        };
        let previous = self.status.borrow();
        let previous_routing = previous.routing.clone();
        let previous_devices = previous.devices.clone();
        drop(previous);
        let mut routing = RoutingStatus {
            priority: self.priority(),
            local_connected: self.hub.borrow().session().is_some(),
            last_switch_reason: previous_routing.last_switch_reason.clone(),
        };
        if routing.priority != previous_routing.priority {
            routing.last_switch_reason = Some(reason.into());
        }
        let dids = self
            .directory
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
                if self.direct_candidate(&did).is_ok() {
                    usable.push(PathKind::Direct);
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
            if self.direct_candidate(&did).is_ok() {
                usable_read.push(PathKind::Direct);
            }
            if self.cloud_observation_candidate(&did, true).is_ok() {
                usable_read.push(PathKind::Cloud);
            }
            let selected_read = self.read_path(&did).map(path_kind);
            let physical_online =
                if self.has_sensors(&did) && super::routing::unreliable_cloud_online(&did) {
                    None
                } else {
                    let live = self.directory.borrow();
                    let slot = live.devices.get(&did).unwrap();
                    slot.cloud_access
                        .as_ref()
                        .map(|access| access.online)
                        .or_else(|| {
                            self.hub
                                .borrow()
                                .access(&did)
                                .and_then(|access| access.as_ref().ok())
                                .map(|access| access.online)
                        })
                };
            let notification = self.active_notification(&did);
            let old = previous_devices.iter().find(|device| device.did == did);
            let switched = old.is_some_and(|old| {
                old.selected_control != selected
                    || old.selected_read != selected_read
                    || old.notification_source != notification
            });
            let last_switch_reason = {
                let mut live = self.directory.borrow_mut();
                let slot = live.devices.get_mut(&did).unwrap();
                if switched {
                    slot.last_switch_reason = Some(reason.into());
                }
                slot.last_switch_reason.clone()
            };
            let unavailable_reason = (!self.device_path_available(&did)).then(|| {
                format!(
                    "hub: {}; direct: {}; cloud: {}",
                    self.local_candidate(&did, false)
                        .err()
                        .expect("no local device path"),
                    self.direct_candidate(&did)
                        .err()
                        .expect("no direct device path"),
                    self.cloud_observation_candidate(&did, true)
                        .expect_err("no cloud device path")
                )
            });
            let sync_limited_reason = notification
                .is_none()
                .then(|| self.notification_reason(&did));
            let work = self.directory.borrow().devices[&did]
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
                    let notification_source =
                        if self.is_virtual_group(&did) && function.lighting().is_some() {
                            notification
                        } else {
                            work.function_observer(function.id())
                                .filter(|source| self.source_valid_for(&did, function.id(), source))
                                .map(|source| match source {
                                    super::notifications::NotificationSource::Local(_) => {
                                        PathKind::Local
                                    }
                                    super::notifications::NotificationSource::Cloud(_) => {
                                        PathKind::Cloud
                                    }
                                })
                        };
                    FunctionStatus {
                        function: function.id().to_owned(),
                        selected_read: self.read_path_for(&did, function.id()).map(path_kind),
                        low_speed: function.fan().and_then(|fan| fan.shared_speed()).map(|_| {
                            LowSpeedStatus {
                                candidates: self.low_speed_candidate_paths(&did, function.id()),
                                selected: self.low_speed_path(&did, function.id()).map(path_kind),
                            }
                        }),
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
                        properties: if let Some((mode, _)) = function.relay_mode() {
                            property_statuses(
                                &work,
                                function.id(),
                                [("Power", function.power().unwrap()), ("Mode", mode)],
                            )
                        } else if let Some(ac) = function.air_conditioner() {
                            let state = ac.mcn04().map(|patch| patch.state);
                            let mut bindings = vec![
                                ("Power", Some(state.unwrap_or(ac.power()))),
                                ("Mode", Some(state.unwrap_or(ac.mode().property()))),
                                (
                                    "Temperature",
                                    Some(state.unwrap_or(ac.temperature().property())),
                                ),
                                ("Speed", Some(state.unwrap_or(ac.speed().property()))),
                                (
                                    "Vertical swing",
                                    ac.swing().map(|swing| state.unwrap_or(swing)),
                                ),
                            ];
                            if let Some(patch) = ac.mcn04() {
                                bindings.push(("Control range", Some(patch.range)));
                            }
                            property_statuses(
                                &work,
                                function.id(),
                                bindings.into_iter().filter_map(|(name, binding)| {
                                    binding.map(|binding| (name, binding))
                                }),
                            )
                        } else if let Some(heater) = function.heater() {
                            property_statuses(
                                &work,
                                function.id(),
                                [
                                    ("Power", heater.power()),
                                    ("Temperature", heater.temperature().property()),
                                ],
                            )
                        } else if let Some(fan) = function.fan() {
                            property_statuses(
                                &work,
                                function.id(),
                                [
                                    ("Power", Some(fan.power())),
                                    ("Native mode", fan.mode().map(|mode| mode.property())),
                                    ("Speed", fan.standard_speed().map(|speed| speed.property())),
                                    ("Horizontal swing", fan.swing()),
                                ]
                                .into_iter()
                                .filter_map(|(name, binding)| {
                                    binding.map(|binding| (name, binding))
                                }),
                            )
                        } else if let Some(curtain) = function.curtain() {
                            property_statuses(
                                &work,
                                function.id(),
                                [
                                    ("Current position", curtain.current().property()),
                                    ("Target position", curtain.target().property()),
                                    ("Status", curtain.status().property()),
                                ],
                            )
                        } else {
                            Vec::new()
                        },
                    }
                })
                .collect();
            let battery_problem = work.battery_problem();
            let standard_mode = work.native.plan().standard_mode().and_then(|mode| {
                work.standard_mode_value()
                    .map(|value| DiagnosticObservation {
                        value: value.value,
                        source: work.property_source(&mode.function, mode.binding),
                        source_time: value.source_time,
                        received_at: Some(value.received_at),
                        reason: value.reason,
                    })
            });
            let shared_feedback = work.native.plan().shared_feedback().map(|plan| {
                let state = work.shared_feedback();
                let field = |value: &Option<super::model::NativeValueObservation>| {
                    value.as_ref().map(|value| DiagnosticObservation {
                        value: value.value.clone(),
                        source: state.source,
                        source_time: value.source_time,
                        received_at: Some(value.received_at),
                        reason: value.reason.clone(),
                    })
                };
                SharedFeedbackStatus {
                    candidates: self.shared_feedback_candidate_paths(&did),
                    selected_read: self.shared_feedback_read_path(&did).map(path_kind),
                    source: state.source,
                    pending: state.pending,
                    read_problem: work.refresh.borrow().input_problem(
                        crate::xiaomi::capability::SHARED_FEEDBACK_CHANNEL,
                        crate::xiaomi::capability::ReadInput::SharedFeedback,
                    ),
                    native_mode: field(&state.native_mode),
                    fan_speed_index: field(&state.fan_speed_index),
                    temperature: field(&state.temperature),
                    fans: plan
                        .fans
                        .iter()
                        .map(|fan| {
                            let snapshot = work
                                .physical()
                                .function(&fan.function)
                                .and_then(|function| function.snapshot().fan())
                                .map(|fan| fan.speed);
                            SharedFanSpeedStatus {
                                function: fan.function.clone(),
                                channel: fan.channel,
                                level: snapshot.and_then(|speed| speed.value),
                                reason: state.fan_reasons.get(&fan.channel).cloned(),
                                source_time: snapshot.and_then(|speed| speed.source_time),
                                received_at: snapshot.and_then(|speed| speed.received_at),
                            }
                        })
                        .collect(),
                }
            });
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
            let lan = self.directory.borrow().devices[&did].lan.status.clone();
            devices.push(DeviceStatus {
                did,
                lan,
                direct_unavailable_reason: self.direct_candidate(&work.key.id).err(),
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
                standard_mode,
                shared_feedback,
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
        self.directory
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
        let work = self.directory.borrow().devices[did].admitted.clone();
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
        format!("hub notifications: {local}; cloud notifications: {cloud}")
    }
    pub(super) fn request_sync(&self) {
        self.sync_requested.set(true);
        self.sync_changed.notify(usize::MAX);
    }
    pub(super) fn set_gateway(&self, status: GatewayStatus) {
        let changed = self.hub.borrow_mut().gateway_changed(status);
        if changed {
            log::info!("Xiaomi gateway state: {:?}", self.hub.borrow().gateway());
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn set_catalog_problem(&self, problem: Option<String>) {
        if self.directory.borrow_mut().catalog_result(problem) {
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn set_access_problem(&self, problem: Option<String>) {
        if self.hub.borrow_mut().access_result(problem) {
            self.status_changed.notify(usize::MAX);
        }
    }
    fn catalog_problem(&self) -> Option<String> {
        let directory = self.directory.borrow();
        let hub = self.hub.borrow();
        let problems = [directory.problem(), hub.problem()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        (!problems.is_empty()).then(|| problems.join("; "))
    }
    pub(super) fn problem(&self, did: &str, reason: Option<String>) {
        let name = self
            .hub
            .borrow()
            .access(did)
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

fn property_statuses(
    work: &DeviceWork,
    id: &str,
    bindings: impl IntoIterator<Item = (&'static str, PropertyBinding)>,
) -> Vec<ObservedPropertyStatus> {
    let subscriptions = work.subscription_status(id);
    let refresh = work.refresh.borrow();
    bindings
        .into_iter()
        .map(|(name, binding)| ObservedPropertyStatus {
            name,
            binding,
            observation_source: work.property_source(id, binding),
            native_mode: (name == "Native mode").then(|| work.fan_mode_observation(id)),
            read_problem: work
                .mcn04_problem(id, binding)
                .or_else(|| refresh.property_problem(id, binding)),
            source_detail: (name == "Control range")
                .then(|| work.mcn04_range(id))
                .flatten(),
            subscriptions: subscriptions
                .iter()
                .filter(|item| item.binding == SubscriptionBinding::Property(binding))
                .cloned()
                .collect(),
        })
        .collect()
}
