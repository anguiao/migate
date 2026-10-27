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
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        if let Some(work) = &work
            && let Some(function) = work.native.device().functions().iter().find(|function| {
                function
                    .state()
                    .is_some_and(|state| state.property() == binding)
            })
        {
            if let Ok(active) = function.state().unwrap().read_value(value)
                && self.accept_state_notification(work, function.id(), source)
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
                self.update_routing("state property observed");
                self.status_changed.notify(usize::MAX);
            }
            return;
        }
        if let Some(work) = work
            && let Some(id) = work.binding_channel(binding)
            && self.source_valid_for(did, id, source)
            && work
                .function_observer(id)
                .is_some_and(|active| active.same(source))
        {
            let battery_problem = work.battery_problem();
            let changed = work.notify_value_at(
                binding,
                value,
                update_time,
                cached_report,
                self.is_independent_light(did),
                self.clock.unix_now(),
            );
            if work.battery_problem() != battery_problem {
                self.status_changed.notify(usize::MAX);
            }
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
    ) -> bool {
        let did = &work.key.id;
        if !self.source_valid_for(did, id, source) {
            return false;
        }
        if work
            .function_observer(id)
            .is_some_and(|active| active.same(source))
        {
            return true;
        }
        if self
            .notification_source(did, id)
            .is_some_and(|desired| desired.same(source))
            && work.source_ready(id, source)
        {
            work.install_state_observer(id, source.clone());
            self.update_device_availability(did);
            self.note_notification_switch(did, "state notification established source baseline");
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
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.clone());
        if let Some(work) = work
            && let Some(function) = work.event_function(binding)
            && self.accept_state_notification(&work, function.id(), source)
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
            self.update_routing("native motion event observed");
            self.status_changed.notify(usize::MAX);
        }
    }
    pub(super) fn online_event(&self, handle: &NotificationHandle, did: &str, online: bool) {
        if !self.current_notifications(handle) {
            return;
        }
        if let Some(slot) = self.live.borrow_mut().devices.get_mut(did) {
            if let Some(access) = &mut slot.cloud_access {
                if online != access.online
                    && let Some(work) = &slot.admitted
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
        self.update_routing("cloud online conditions changed");
        self.changed.notify(usize::MAX);
        self.refresh_changed.notify(usize::MAX);
    }
}
