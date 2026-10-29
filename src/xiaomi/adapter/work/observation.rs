use super::*;

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn reconcile_state_reads(&self, context: &Context) {
        for function in self.native.device().functions().iter().filter(|function| {
            function.state().is_some() || function.curtain().is_some() || function.fan().is_some()
        }) {
            let id = function.id();
            let path = context.read_path_for(&self.key.id, id);
            let changed = {
                let mut paths = self.state_read_paths.borrow_mut();
                let changed =
                    paths
                        .get(id)
                        .is_some_and(|old| match (old.as_ref(), path.as_ref()) {
                            (Some(old), Some(new)) => !old.same(new),
                            (None, None) => false,
                            _ => true,
                        });
                paths.insert(id.into(), path.clone());
                changed
            };
            if changed {
                for binding in function.observation_bindings() {
                    super::super::refresh::reconcile_state_read(
                        self,
                        context,
                        id,
                        binding,
                        path.is_some(),
                    );
                }
            }
        }
    }
    pub(in crate::xiaomi::adapter) fn observation_source(&self, id: &str) -> Option<PathKind> {
        self.observation_sources.borrow().get(id).copied()
    }
    pub(in crate::xiaomi::adapter) fn property_source(
        &self,
        id: &str,
        binding: PropertyBinding,
    ) -> Option<PathKind> {
        self.property_sources
            .borrow()
            .get(&super::super::observations::key(id, binding))
            .copied()
    }
    pub(in crate::xiaomi::adapter) fn event_function(
        &self,
        binding: EventBinding,
    ) -> Option<&ParsedFunction> {
        self.native.device().functions().iter().find(|function| {
            function
                .motion()
                .is_some_and(|motion| motion.event() == binding)
        })
    }
    pub(in crate::xiaomi::adapter) fn state_function(&self, id: &str) -> bool {
        self.native
            .device()
            .functions()
            .iter()
            .any(|function| function.id() == id && function.state().is_some())
    }
    pub(in crate::xiaomi::adapter) fn observe_state(
        &self,
        function: &ParsedFunction,
        active: Option<bool>,
        input: crate::device::StateInput,
        observation: Observation,
        origin: Origin,
    ) {
        let state = function.state().unwrap();
        self.observations
            .observed(self, function.id(), state.property(), origin);
        if !matches!(origin, Origin::Read) {
            self.refresh
                .borrow_mut()
                .satisfy_baseline(function.id(), state.property());
        }
        self.physical()
            .function(function.id())
            .unwrap()
            .observe_state(
                active,
                input,
                observation.source_time,
                observation.received_at,
            )
            .expect("state function");
        self.observation_sources
            .borrow_mut()
            .insert(function.id().into(), observation.source);
    }
    pub(in crate::xiaomi::adapter) fn binding_channel(
        &self,
        binding: PropertyBinding,
    ) -> Option<&str> {
        if self.battery_binding() == Some(binding) {
            return Some(BATTERY_CHANNEL);
        }
        self.native
            .device()
            .functions()
            .iter()
            .find(|function| {
                function.observation_bindings().contains(&binding)
                    || function.kind().is_control()
                        && self.native.device().refresh_trigger() == Some(binding)
            })
            .map(|function| function.id())
    }
    pub(in crate::xiaomi::adapter) fn notify_value_at(
        &self,
        binding: PropertyBinding,
        value: &serde_json::Value,
        update_time: Option<i64>,
        cached_report: bool,
        takeover: bool,
        received_at: i64,
    ) -> bool {
        if self.life.is_closed() {
            return false;
        }
        if self.native.device().refresh_trigger() == Some(binding) && value.as_bool().is_some() {
            self.refresh();
            return false;
        }
        if self.battery_binding() == Some(binding) {
            let origin = if cached_report {
                Origin::Cache
            } else {
                Origin::Notification
            };
            return match observe_battery(self, binding, value, update_time, received_at, origin) {
                Ok(()) => false,
                Err(error) => {
                    log::warn!("Ignoring invalid battery property: {error}");
                    self.set_battery_read_problem(Some(format!(
                        "invalid battery property: {error}"
                    )));
                    false
                }
            };
        }
        let Some(function) = self.native.device().functions().iter().find(|function| {
            function.curtain().is_none()
                && function.fan().is_none()
                && function.observation_bindings().contains(&binding)
        }) else {
            return false;
        };
        let id = function.id().to_owned();
        let light = function.lighting().is_some();
        if light {
            let origin = if cached_report {
                Origin::Cache
            } else {
                Origin::Notification
            };
            match super::super::lighting::observe(self, &id, binding, value, origin, takeover) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("Ignoring invalid light property: {error}");
                    false
                }
            }
        } else if function.measurement().is_some() {
            let origin = if cached_report {
                Origin::Cache
            } else {
                Origin::Notification
            };
            match observe_measurement(
                self,
                function,
                binding,
                value,
                update_time,
                received_at,
                origin,
            ) {
                Ok(()) => false,
                Err(error) => {
                    log::warn!("Ignoring invalid measurement property: {error}");
                    false
                }
            }
        } else {
            if let Some(value) = value.as_bool() {
                self.observations.notify(self, &id, value);
            }
            false
        }
    }
    pub(in crate::xiaomi::adapter) fn observe_read_at(
        &self,
        id: &str,
        binding: PropertyBinding,
        stamp: &super::super::observations::ReadStamp,
        value: &serde_json::Value,
        observation: Observation,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        let Observation {
            source_time: update_time,
            received_at,
            ..
        } = observation;
        if !self.observations.read_is_current(self, id, binding, stamp) {
            return Ok(());
        }
        if id == BATTERY_CHANNEL && self.battery_binding() == Some(binding) {
            return observe_battery(self, binding, value, update_time, received_at, Origin::Read);
        }
        let function = self
            .native
            .device()
            .functions()
            .iter()
            .find(|function| function.id() == id)
            .unwrap();
        if function.fan().is_some() {
            let decoded = Self::decode_fan(function, binding, value)?;
            self.observe_fan(function, binding, decoded, observation, Origin::Read);
        } else if function.curtain().is_some() {
            let decoded = Self::decode_curtain(function, binding, value)?;
            self.observe_curtain(function, binding, decoded, observation, Origin::Read);
        } else if function.lighting().is_some() {
            super::super::lighting::observe(self, id, binding, value, Origin::Read, false)?;
        } else if function.measurement().is_some() {
            observe_measurement(
                self,
                function,
                binding,
                value,
                update_time,
                received_at,
                Origin::Read,
            )?;
        } else if let Some(state) = function.state() {
            let active = state.read_value(value)?;
            self.observe_state(
                function,
                active,
                crate::device::StateInput::Property(value.clone()),
                observation,
                Origin::Read,
            );
        } else {
            let value =
                value
                    .as_bool()
                    .ok_or(crate::xiaomi::capability::CapabilityError::InvalidValue(
                        "boolean",
                    ))?;
            self.observations.observe_read(self, id, stamp, value);
        }
        Ok(())
    }

    pub(in crate::xiaomi::adapter) fn observe_fan(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: crate::device::FanValue,
        observation: Observation,
        origin: Origin,
    ) {
        if self.life.is_closed() {
            return;
        }
        let id = function.id();
        self.observations.observed(self, id, binding, origin);
        if !matches!(origin, Origin::Read) {
            self.refresh.borrow_mut().satisfy_baseline(id, binding);
        }
        let function = self.physical().function(id).unwrap();
        if matches!(origin, Origin::Cache) {
            function.observe_cached_fan(value, observation.source_time, observation.received_at);
        } else {
            function.observe_fan(value, observation.source_time, observation.received_at);
        }
        self.property_sources.borrow_mut().insert(
            super::super::observations::key(id, binding),
            observation.source,
        );
    }
    pub(in crate::xiaomi::adapter) fn decode_fan(
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
    ) -> Result<crate::device::FanValue, crate::xiaomi::capability::CapabilityError> {
        use crate::{device::FanValue, xiaomi::capability::CapabilityError};
        let fan = function.fan().expect("fan function");
        if binding == fan.power() {
            Ok(FanValue::Power(
                value
                    .as_bool()
                    .ok_or(CapabilityError::InvalidValue("fan power"))?,
            ))
        } else if binding == fan.mode().property() {
            Ok(FanValue::Mode(fan.mode().read_value(value)?))
        } else if binding == fan.speed().property() {
            Ok(FanValue::Speed(fan.speed().read_value(value)?))
        } else {
            debug_assert_eq!(Some(binding), fan.swing());
            Ok(FanValue::Swing(
                value
                    .as_bool()
                    .ok_or(CapabilityError::InvalidValue("fan swing"))?,
            ))
        }
    }
    pub(in crate::xiaomi::adapter) fn observe_curtain(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: crate::device::CurtainValue,
        observation: Observation,
        origin: Origin,
    ) {
        if self.life.is_closed() {
            return;
        }
        let id = function.id();
        self.observations.observed(self, id, binding, origin);
        if !matches!(origin, Origin::Read) {
            self.refresh.borrow_mut().satisfy_baseline(id, binding);
        }
        let physical = self.physical();
        let function = physical.function(id).unwrap();
        match origin {
            Origin::Notification => {
                function.observe_curtain(value, observation.source_time, observation.received_at)
            }
            Origin::Read | Origin::Cache => {
                function.read_curtain(value, observation.source_time, observation.received_at)
            }
        }
        self.property_sources.borrow_mut().insert(
            super::super::observations::key(id, binding),
            observation.source,
        );
    }
    pub(in crate::xiaomi::adapter) fn decode_curtain(
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
    ) -> Result<crate::device::CurtainValue, crate::xiaomi::capability::CapabilityError> {
        use crate::device::CurtainValue;
        let curtain = function.curtain().expect("curtain function");
        if binding == curtain.current().property() {
            Ok(CurtainValue::CurrentPosition(
                curtain.current().read_raw(value)?,
            ))
        } else if binding == curtain.target().property() {
            Ok(CurtainValue::TargetPosition(
                curtain.target().read_raw(value)?,
            ))
        } else {
            debug_assert_eq!(binding, curtain.status().property());
            Ok(CurtainValue::Status(curtain.status().read_value(value)?))
        }
    }
}

fn observe_battery(
    work: &DeviceWork,
    binding: PropertyBinding,
    value: &serde_json::Value,
    source_time: Option<i64>,
    received_at: i64,
    origin: Origin,
) -> Result<(), crate::xiaomi::capability::CapabilityError> {
    let battery = work
        .native
        .device()
        .battery()
        .expect("caller selected the battery binding");
    let decoded = battery.read_value(value)?;
    if work.life.is_closed() {
        return Ok(());
    }
    work.observations
        .observed(work, BATTERY_CHANNEL, binding, origin);
    work.physical()
        .observe_battery(decoded, source_time, received_at)
        .map_err(|_| crate::xiaomi::capability::CapabilityError::InvalidValue("battery"))?;
    work.set_battery_read_problem(None);
    Ok(())
}

fn observe_measurement(
    work: &DeviceWork,
    function: &crate::xiaomi::capability::ParsedFunction,
    binding: PropertyBinding,
    value: &serde_json::Value,
    source_time: Option<i64>,
    received_at: i64,
    origin: Origin,
) -> Result<(), crate::xiaomi::capability::CapabilityError> {
    let measurement = function
        .measurement()
        .expect("caller selected a measurement function");
    let decoded = measurement.read_value(value)?;
    if work.life.is_closed() {
        return Ok(());
    }
    let id = function.id();
    work.observations.observed(work, id, binding, origin);
    work.physical()
        .function(id)
        .expect("installed source function")
        .observe_measurement(decoded, source_time, received_at)
        .map_err(|_| crate::xiaomi::capability::CapabilityError::InvalidValue("measurement"))?;
    Ok(())
}
