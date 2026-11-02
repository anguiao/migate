use super::*;
use crate::device::Availability;

impl Context {
    pub(in crate::xiaomi::adapter) fn group_member_work(
        &self,
        did: &str,
    ) -> Vec<(Rc<DeviceWork>, String)> {
        let graph = self.graph.borrow().clone();
        let Some(group) = graph.rules.get(did) else {
            return Vec::new();
        };
        let live = self.live.borrow();
        group
            .members
            .iter()
            .filter_map(|member| {
                live.devices
                    .get(&member.device.id)?
                    .admitted
                    .as_ref()
                    .filter(|work| !work.life.is_closed())
                    .map(|work| (work.clone(), member.id.clone()))
            })
            .collect()
    }
    /// Refresh the actual feedback source, sharing ordinary member work.
    pub(in crate::xiaomi::adapter) fn refresh_source(&self, did: &str) {
        if self.is_virtual_group(did) {
            for (work, _) in self.group_member_work(did) {
                work.refresh();
            }
        } else if let Some(work) = self
            .live
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.admitted.as_ref())
        {
            work.refresh();
        }
    }
    pub(in crate::xiaomi::adapter) fn update_availability(&self, did: &str) {
        self.update_device_availability(did);
        self.refresh_group_states(did);
    }
    pub(in crate::xiaomi::adapter) fn device_path_available(&self, did: &str) -> bool {
        self.control_path(did).is_some()
            || self.observation_path(did).is_some()
            || self
                .live
                .borrow()
                .devices
                .get(did)
                .and_then(|slot| slot.admitted.as_ref())
                .is_some_and(|work| {
                    work.native
                        .device()
                        .functions()
                        .iter()
                        .filter(|function| function.kind().is_read_only())
                        .any(|function| {
                            work.function_observer(function.id()).is_some_and(|source| {
                                self.source_valid_for(did, function.id(), &source)
                            })
                        })
                })
    }
    pub(in crate::xiaomi::adapter) fn update_device_availability(&self, did: &str) {
        let available = self.device_path_available(did);
        let (work, blocked) = {
            let live = self.live.borrow();
            let slot = &live.devices[did];
            (slot.admitted.clone(), slot.blocked.clone())
        };
        let Some(work) = work else {
            return;
        };
        let physical = work.physical();
        for function in physical.functions().iter().filter(|function| {
            matches!(
                function.description().kind,
                crate::device::FunctionKind::Temperature(_)
            )
        }) {
            let id = &function.function_key().id;
            let read_available = self.read_path_for(did, id).is_some();
            let source_available = read_available
                || work
                    .function_observer(id)
                    .is_some_and(|source| self.source_valid_for(did, id, &source));
            let had_source = function
                .snapshot()
                .temperature_source()
                .expect("temperature function")
                != crate::device::TemperatureSourceState::Unavailable;
            if had_source && !source_available {
                super::super::refresh::cancel_function(&work, id);
            }
            function.set_temperature_source_available(source_available);
            if !had_source && source_available && read_available {
                let binding = work
                    .native
                    .device()
                    .functions()
                    .iter()
                    .find(|native| native.id() == id)
                    .and_then(|native| native.measurement())
                    .expect("installed temperature binding")
                    .property();
                work.request_dirty_read(id, binding);
            }
        }
        let was_available = physical.availability() == Availability::Available;
        physical.set_availability(if available {
            Availability::Available
        } else {
            Availability::Unavailable(
                blocked.unwrap_or_else(|| "no usable read or notification path".into()),
            )
        });
        if available && !was_available {
            self.refresh_source(did);
        }
    }
    pub(in crate::xiaomi::adapter) fn update_all_availability(&self) {
        let dids = self
            .live
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for did in dids {
            self.update_device_availability(&did);
        }
        self.refresh_all_group_states();
    }
}
