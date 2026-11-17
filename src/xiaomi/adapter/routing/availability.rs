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
        let live = self.directory.borrow();
        group
            .members
            .iter()
            .filter_map(|member| {
                live.devices
                    .get(&member.device.id)?
                    .work
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
            .directory
            .borrow()
            .devices
            .get(did)
            .and_then(|slot| slot.work.as_ref())
        {
            work.refresh();
        }
    }
    pub(in crate::xiaomi::adapter) fn update_availability(&self, did: &str) {
        self.update_device_availability(did);
        self.refresh_group_states(did);
    }
    pub(in crate::xiaomi::adapter) fn update_device_availability(&self, did: &str) {
        let routes = self.device_routes(did);
        self.apply_device_availability(did, &routes);
    }
    fn apply_device_availability(&self, did: &str, routes: &DeviceRoutes) {
        let available = routes.available;
        let (work, blocked) = {
            let live = self.directory.borrow();
            let slot = &live.devices[did];
            (slot.work.clone(), slot.description_problem())
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
            let function_routes = &routes.functions[id];
            let read_available = function_routes.read.selected.is_some();
            let source_available = read_available || function_routes.notification.active.is_some();
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
            .directory
            .borrow()
            .devices
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let routes = self.device_routes_for(dids.iter().map(String::as_str));
        for did in dids {
            self.apply_device_availability(&did, &routes[&did]);
        }
        self.refresh_all_group_states();
    }
}
