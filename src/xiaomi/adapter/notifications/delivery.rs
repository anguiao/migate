use super::*;

impl Context {
    pub(in crate::xiaomi::adapter) fn property_value_event(
        &self,
        source: &NotificationSource,
        did: &str,
        binding: crate::xiaomi::capability::PropertyBinding,
        value: &serde_json::Value,
        update_time: Option<i64>,
        cached_report: bool,
    ) {
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let routes = self.device_routes(did);
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function
                    .relay_mode()
                    .is_some_and(|(mode, _)| mode == binding)
            })
        {
            let id = function.id();
            let active = work.function_observer(id);
            let notification = &routes.functions[id].notification;
            let desired = &notification.desired;
            let selected = active.as_ref().is_some_and(|active| active.same(source))
                || desired.as_ref().is_some_and(|desired| desired.same(source))
                || active.is_none() && desired.is_none();
            if function.relay_mode().unwrap().1
                && selected
                && routes.functions[id].notification.valid(source)
                && work.source_binding_ready(source, binding)
                && work
                    .observe_relay_mode(
                        function,
                        binding,
                        value,
                        Observation {
                            source: source.kind(),
                            source_time: update_time,
                            received_at: self.clock.unix_now(),
                        },
                        if cached_report {
                            Origin::Cache
                        } else {
                            Origin::Notification
                        },
                    )
                    .is_ok()
            {
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if !cached_report
            && let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function.relay_mode().is_some() && function.power() == Some(binding)
            })
            && let Some(value) = value.as_bool()
            && routes.functions[function.id()].notification.valid(source)
            && work
                .function_observer(function.id())
                .is_some_and(|active| active.same(source))
        {
            if work.observe_relay_power_notification(function.id(), value, source.kind()) {
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some((id, power, trigger)) =
                work.native
                    .device()
                    .functions()
                    .iter()
                    .find_map(|function| {
                        let heater = function.heater()?;
                        let trigger = heater.power_refresh_trigger()?;
                        (trigger.property == binding)
                            .then(|| (function.id(), heater.power(), trigger))
                    })
        {
            if !cached_report
                && trigger.accepts(value)
                && self.accept_state_notification(work, id, source, &routes)
            {
                work.refresh_property(id, power);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work
                .native
                .device()
                .functions()
                .iter()
                .find(|function| work.is_standard_mode_binding(function.id(), binding))
        {
            if self.accept_state_notification(work, function.id(), source, &routes)
                && let Some(valid) = work.observe_standard_mode(
                    function,
                    binding,
                    value,
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                )
            {
                if !cached_report
                    && valid
                    && let Some(scope) = work.related_scope_for_notification(function.id(), binding)
                {
                    work.refresh_related_inputs(scope);
                }
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function.air_conditioner().is_some()
                    && function.observation_bindings().contains(&binding)
            })
        {
            if function.air_conditioner().unwrap().mcn04().is_some() {
                if self.accept_state_notification(work, function.id(), source, &routes) {
                    let _ = work.observe_mcn04(
                        function,
                        binding,
                        value,
                        Observation {
                            source: source.kind(),
                            source_time: update_time,
                            received_at: self.clock.unix_now(),
                        },
                        if cached_report {
                            Origin::Cache
                        } else {
                            Origin::Notification
                        },
                    );
                    self.status_changed.notify(usize::MAX);
                }
                return;
            }
            if let Ok(value) = DeviceWork::decode_air_conditioner(function, binding, value)
                && self.accept_state_notification(work, function.id(), source, &routes)
            {
                work.observe_air_conditioner(
                    function,
                    binding,
                    value,
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                );
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function.heater().is_some() && function.observation_bindings().contains(&binding)
            })
        {
            if let Ok(value) = DeviceWork::decode_heater(function, binding, value)
                && self.accept_state_notification(work, function.id(), source, &routes)
            {
                let applied = work.observe_heater(
                    function,
                    binding,
                    value,
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                );
                if !cached_report
                    && applied
                    && let Some(scope) = work.related_scope_for_notification(function.id(), binding)
                {
                    work.refresh_related_inputs(scope);
                }
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function.fan().is_some() && function.observation_bindings().contains(&binding)
            })
        {
            if let Ok(value) = DeviceWork::decode_fan(function, binding, value)
                && self.accept_state_notification(work, function.id(), source, &routes)
            {
                let applied = work.observe_fan(
                    function,
                    binding,
                    value,
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                );
                if !cached_report
                    && applied
                    && let Some(scope) = work.related_scope_for_notification(function.id(), binding)
                {
                    work.refresh_related_inputs(scope);
                }
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function.curtain().is_some() && function.observation_bindings().contains(&binding)
            })
        {
            if let Ok(value) = DeviceWork::decode_curtain(function, binding, value)
                && self.accept_state_notification(work, function.id(), source, &routes)
            {
                work.observe_curtain(
                    function,
                    binding,
                    value,
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                );
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function
                    .state()
                    .is_some_and(|state| state.property() == binding)
            })
        {
            if let Ok(active) = function.state().unwrap().read_value(value)
                && self.accept_state_notification(work, function.id(), source, &routes)
            {
                work.observe_state(
                    function,
                    active,
                    crate::device::StateInput::Property(value.clone()),
                    Observation {
                        source: source.kind(),
                        source_time: update_time,
                        received_at: self.clock.unix_now(),
                    },
                    if cached_report {
                        Origin::Cache
                    } else {
                        Origin::Notification
                    },
                );
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = work
            && let Some(id) = work.binding_channel(binding)
            && routes.functions[id].notification.valid(source)
            && work
                .function_observer(id)
                .is_some_and(|active| active.same(source))
        {
            let changed = work.notify_value_at(
                binding,
                value,
                update_time,
                cached_report,
                self.is_independent_light(did, id),
                self.clock.unix_now(),
            );
            self.status_changed.notify(usize::MAX);
            if changed && !self.is_group(did) {
                self.refresh_group_states(did);
            }
        }
    }
    fn accept_state_notification(
        &self,
        work: &DeviceWork,
        id: &str,
        source: &NotificationSource,
        routes: &super::super::routing::DeviceRoutes,
    ) -> bool {
        let did = &work.key.id;
        let notification = &routes.functions[id].notification;
        if !notification.valid(source) {
            return false;
        }
        if work
            .function_observer(id)
            .is_some_and(|active| active.same(source))
        {
            return true;
        }
        if notification
            .desired
            .as_ref()
            .is_some_and(|desired| desired.same(source))
            && work.source_ready(id, source)
        {
            if work.native.device().functions().iter().any(|function| {
                function.id() == id
                    && (function.curtain().is_some()
                        || function.fan().is_some()
                        || function.air_conditioner().is_some()
                        || function.heater().is_some())
            }) {
                work.install_observer(id, source.clone());
            } else {
                work.install_state_observer(id, source.clone());
            }
            self.update_device_availability(did);
            self.note_notification_switch(
                did,
                id,
                "state notification established source baseline",
            );
            return true;
        }
        false
    }
    pub(in crate::xiaomi::adapter) fn native_event(
        &self,
        source: &NotificationSource,
        did: &str,
        binding: crate::xiaomi::capability::EventBinding,
        source_time: Option<i64>,
    ) {
        let work = self
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.clone());
        let routes = self.device_routes(did);
        if let Some(work) = work
            && let Some(function) = work.event_function(binding)
            && self.accept_state_notification(&work, function.id(), source, &routes)
        {
            work.observe_state(
                function,
                Some(true),
                crate::device::StateInput::Event {
                    siid: binding.siid,
                    eiid: binding.eiid,
                },
                Observation {
                    source: source.kind(),
                    source_time,
                    received_at: self.clock.unix_now(),
                },
                Origin::Notification,
            );
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn online_event(&self, handle: &NotificationHandle, did: &str, online: bool) {
        if !self.current_notifications(handle) {
            return;
        }
        if let Some(slot) = self.directory.borrow_mut().devices.get_mut(did) {
            if let Some(access) = &mut slot.cloud_access {
                if online != access.online
                    && let Some(work) = &slot.work
                {
                    work.retry_subscriptions();
                }
                access.online = online;
            } else {
                self.request_sync();
                return;
            }
        } else {
            return;
        }
        // Online reports only update this path's access, never HTTP/auth suspension.
        self.update_availability(did);
        self.update_device_routing(did, "cloud online conditions changed");
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
    }
}
