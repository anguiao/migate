use super::{
    model::{
        AdapterStatus, DeviceStatus, DiagnosticObservation, FunctionStatus, GatewayStatus,
        LowSpeedStatus, ObservedPropertyStatus, RoutingStatus, SharedFanSpeedStatus,
        SharedFeedbackStatus, SubscriptionBinding, SubscriptionStatus,
    },
    state::Context,
    work::DeviceWork,
};
use crate::xiaomi::capability::PropertyBinding;

pub(super) mod history;

impl Context {
    pub(super) fn diagnostics(&self) -> AdapterStatus {
        let now = self.clock.unix_now();
        let evaluated = self.routing_evaluation_at(now);
        let routing = RoutingStatus {
            priority: evaluated.summaries.priority,
            local_connected: evaluated.local_connected,
            last_switch_reason: self.routing_history.borrow().reason.clone(),
        };
        let directory = self.directory.borrow();
        let mut devices = Vec::with_capacity(evaluated.devices.len());
        for (did, routes) in evaluated.devices {
            let slot = &directory.devices[&did];
            let work = slot.admitted.as_ref().unwrap();
            let last_switch_reason = evaluated.summaries.devices[&did]
                .deadline
                .map(|deadline| deadline.reason().into())
                .or_else(|| slot.last_switch_reason.clone());
            let functions = work
                .native
                .device()
                .functions()
                .iter()
                .map(|function| {
                    let function_routes = &routes.functions[function.id()];
                    let group_light = routes.virtual_group && function.lighting().is_some();
                    let notification_source = if group_light {
                        routes.notification_source
                    } else {
                        function_routes
                            .notification
                            .active
                            .as_ref()
                            .map(|source| source.kind())
                    };
                    let subscriptions = work.subscription_status(function.id());
                    let properties = if let Some((mode, _)) = function.relay_mode() {
                        property_statuses(
                            work,
                            function.id(),
                            &subscriptions,
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
                            work,
                            function.id(),
                            &subscriptions,
                            bindings.into_iter().filter_map(|(name, binding)| {
                                binding.map(|binding| (name, binding))
                            }),
                        )
                    } else if let Some(heater) = function.heater() {
                        property_statuses(
                            work,
                            function.id(),
                            &subscriptions,
                            [
                                ("Power", heater.power()),
                                ("Temperature", heater.temperature().property()),
                            ],
                        )
                    } else if let Some(fan) = function.fan() {
                        property_statuses(
                            work,
                            function.id(),
                            &subscriptions,
                            [
                                ("Power", Some(fan.power())),
                                ("Native mode", fan.mode().map(|mode| mode.property())),
                                ("Speed", fan.standard_speed().map(|speed| speed.property())),
                                ("Horizontal swing", fan.swing()),
                            ]
                            .into_iter()
                            .filter_map(|(name, binding)| binding.map(|binding| (name, binding))),
                        )
                    } else if let Some(curtain) = function.curtain() {
                        property_statuses(
                            work,
                            function.id(),
                            &subscriptions,
                            [
                                ("Current position", curtain.current().property()),
                                ("Target position", curtain.target().property()),
                                ("Status", curtain.status().property()),
                            ],
                        )
                    } else {
                        Vec::new()
                    };
                    FunctionStatus {
                        function: function.id().to_owned(),
                        selected_read: function_routes.read.selected_kind(),
                        low_speed: function_routes.low_speed.as_ref().map(|choice| {
                            LowSpeedStatus {
                                candidates: choice.candidates.kinds(),
                                selected: choice.selected_kind(),
                            }
                        }),
                        notification_source,
                        sync_limited_reason: if group_light {
                            routes.sync_limited_reason.clone()
                        } else {
                            function_routes.notification.sync_limited_reason.clone()
                        },
                        read_problem: work.refresh.borrow().problem_for(function.id()),
                        observation_source: work.observation_source(function.id()),
                        subscriptions: subscriptions
                            .into_iter()
                            .filter(|subscription| {
                                !properties.iter().any(|property| {
                                    subscription.binding
                                        == SubscriptionBinding::Property(property.binding)
                                })
                            })
                            .collect(),
                        properties,
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
                let route = routes.shared_feedback.as_ref().unwrap();
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
                    candidates: route.candidates.kinds(),
                    selected_read: route.selected_kind(),
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
                .map(|function| function.id().to_owned());
            let mut commands = self.commands.snapshot(&did, channels);
            for command in &mut commands {
                command.light_actions = work
                    .physical()
                    .function(&command.channel)
                    .unwrap()
                    .light_action_diagnostics();
            }
            devices.push(DeviceStatus {
                did,
                lan: slot.lan.status.clone(),
                read_only: routes.read_only,
                usable_control_paths: routes.control.candidates.kinds(),
                selected_control: routes.control.selected_kind(),
                usable_read_paths: routes.read.candidates.kinds(),
                selected_read: routes.read.selected_kind(),
                direct_unavailable_reason: routes.read.candidates.direct.err(),
                physical_online: routes.physical_online,
                notification_source: routes.notification_source,
                unavailable_reason: routes.unavailable_reason,
                sync_limited_reason: routes.sync_limited_reason,
                battery_problem,
                last_switch_reason,
                standard_mode,
                shared_feedback,
                commands,
                functions,
            });
        }
        let credentials = self.credentials.borrow();
        AdapterStatus {
            certificate_status: credentials.certificate_status(now),
            auth: credentials.report(now),
            gateway: self.hub.borrow().gateway().clone(),
            catalog_problem: self.catalog_problem(),
            pending: self.pending_devices(),
            routing,
            devices,
        }
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
}

fn property_statuses(
    work: &DeviceWork,
    id: &str,
    subscriptions: &[SubscriptionStatus],
    bindings: impl IntoIterator<Item = (&'static str, PropertyBinding)>,
) -> Vec<ObservedPropertyStatus> {
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
