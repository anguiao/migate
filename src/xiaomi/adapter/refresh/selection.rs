use super::*;
use crate::xiaomi::capability::{ReadInput, SHARED_FEEDBACK_CHANNEL};

fn same_path(previous: Option<&ControlPath>, next: Option<&ControlPath>) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => previous.same(next),
        (None, None) => true,
        _ => false,
    }
}

impl RefreshState {
    fn select_function_path(&mut self, id: &str, path: Option<ControlPath>) -> bool {
        let changed = self
            .function_paths
            .get(id)
            .is_some_and(|previous| !same_path(previous.as_ref(), path.as_ref()));
        self.function_paths.insert(id.to_owned(), path);
        changed
    }

    fn select_shared_path(&mut self, path: Option<ControlPath>) -> bool {
        // The first evaluation establishes availability even without a route.
        let changed = self
            .shared_path
            .as_ref()
            .is_none_or(|previous| !same_path(previous.as_ref(), path.as_ref()));
        self.shared_path = Some(path);
        changed
    }

    pub(in crate::xiaomi::adapter) fn shared_read_available(&self) -> Option<bool> {
        self.shared_path.as_ref().map(Option::is_some)
    }
}

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn reconcile_state_reads(&self, context: &Context) {
        let routes = context.device_routes(&self.key.id);
        for function in self.native.device().functions().iter().filter(|function| {
            function.state().is_some()
                || function.curtain().is_some()
                || function.fan().is_some()
                || function.air_conditioner().is_some()
                || function.heater().is_some()
                || function.relay_mode().is_some()
        }) {
            let id = function.id();
            let path = routes.read_path_for(id);
            let changed = self
                .refresh
                .borrow_mut()
                .select_function_path(id, path.cloned());
            if changed {
                self.current_sink().require_fan_power_read(id);
                if function
                    .air_conditioner()
                    .is_some_and(|ac| ac.mcn04().is_some())
                {
                    self.physical()
                        .function(id)
                        .unwrap()
                        .air_conditioner_source_changed();
                }
                if function.heater().is_some() {
                    self.physical()
                        .function(id)
                        .unwrap()
                        .heater_source_changed();
                }
                for binding in function.observation_bindings() {
                    super::reconcile_input(self, id, ReadInput::Property(binding), path);
                }
            }
        }
        self.reconcile_shared_feedback(&routes);
    }

    fn reconcile_shared_feedback(&self, routes: &DeviceRoutes) {
        if !self.shared_feedback_plan() {
            return;
        }
        let path = routes.read_path_for_input(SHARED_FEEDBACK_CHANNEL, ReadInput::SharedFeedback);
        let available = path.is_some();
        if !self.refresh.borrow_mut().select_shared_path(path.cloned()) {
            return;
        }
        self.set_shared_temperature_source_available(available);
        super::reconcile_input(
            self,
            SHARED_FEEDBACK_CHANNEL,
            ReadInput::SharedFeedback,
            path,
        );
    }
}
