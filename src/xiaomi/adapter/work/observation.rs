pub(in crate::xiaomi::adapter) enum FanObservation {
    Value(crate::device::FanValue),
    Mode(u32),
}

use super::*;

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn reconcile_state_reads(&self, context: &Context) {
        for function in self.native.device().functions().iter().filter(|function| {
            function.state().is_some()
                || function.curtain().is_some()
                || function.fan().is_some()
                || function.air_conditioner().is_some()
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
                if function
                    .air_conditioner()
                    .is_some_and(|ac| ac.mcn04().is_some())
                {
                    self.physical()
                        .function(id)
                        .unwrap()
                        .air_conditioner_source_changed();
                }
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
        if !self
            .observations
            .permits(function.id(), state.property(), origin)
        {
            return;
        }
        self.observations
            .observed(self, function.id(), state.property(), origin);
        if matches!(origin, Origin::Notification) {
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
                && function.air_conditioner().is_none()
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
                let origin = if cached_report {
                    Origin::Cache
                } else {
                    Origin::Notification
                };
                self.observations.notify(self, &id, value, origin);
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
        if function
            .air_conditioner()
            .is_some_and(|ac| ac.mcn04().is_some())
        {
            self.observe_mcn04(function, binding, value, observation, Origin::Read)?;
        } else if function.air_conditioner().is_some() {
            let decoded = Self::decode_air_conditioner(function, binding, value)?;
            self.observe_air_conditioner(function, binding, decoded, observation, Origin::Read);
        } else if function.fan().is_some() {
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

    pub(in crate::xiaomi::adapter) fn mcn04_problem(
        &self,
        id: &str,
        binding: PropertyBinding,
    ) -> Option<String> {
        self.mcn04_problems
            .borrow()
            .get(&super::super::observations::key(id, binding))
            .cloned()
    }
    pub(in crate::xiaomi::adapter) fn mcn04_range(&self, id: &str) -> Option<String> {
        self.mcn04_ranges.borrow().get(id).cloned()
    }
    pub(in crate::xiaomi::adapter) fn observe_mcn04(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
        observation: Observation,
        origin: Origin,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        if self.life.is_closed() || matches!(origin, Origin::Cache) {
            return Ok(());
        }
        let ac = function.air_conditioner().unwrap();
        let patch = ac.mcn04().unwrap();
        let id = function.id();
        let physical = self.physical().function(id).unwrap();
        let key = super::super::observations::key(id, binding);
        let result = if binding == patch.state {
            super::super::mcn04::state(ac, value).map(|report| {
                physical.observe_air_conditioner_report(
                    &report.values,
                    observation.source_time,
                    observation.received_at,
                );
                report.problem
            })
        } else {
            debug_assert_eq!(binding, patch.range);
            super::super::mcn04::range(ac, value).map(|constraints| {
                physical.set_air_conditioner_constraints(constraints);
                self.mcn04_ranges
                    .borrow_mut()
                    .insert(id.to_owned(), value.as_str().unwrap().to_owned());
                None
            })
        };
        let problem = match &result {
            Ok(problem) => problem.clone(),
            Err(error) => Some(error.to_string()),
        };
        if let Some(problem) = problem {
            self.mcn04_problems.borrow_mut().insert(key, problem);
        } else {
            self.mcn04_problems.borrow_mut().remove(&key);
        }
        result?;
        self.observations.observed(self, id, binding, origin);
        if matches!(origin, Origin::Notification) {
            self.refresh.borrow_mut().satisfy_baseline(id, binding);
        }
        self.property_sources.borrow_mut().insert(
            super::super::observations::key(id, binding),
            observation.source,
        );
        Ok(())
    }
    pub(in crate::xiaomi::adapter) fn observe_air_conditioner(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: crate::device::AirConditionerValue,
        observation: Observation,
        origin: Origin,
    ) {
        if self.life.is_closed() {
            return;
        }
        let id = function.id();
        if !self.observations.permits(id, binding, origin) {
            return;
        }
        let function = self.physical().function(id).unwrap();
        if matches!(origin, Origin::Cache) {
            // A retained projection must not cancel or stale fresh baseline reads.
            function.observe_cached_air_conditioner(
                value,
                observation.source_time,
                observation.received_at,
            );
        } else {
            self.observations.observed(self, id, binding, origin);
            if matches!(origin, Origin::Notification) {
                self.refresh.borrow_mut().satisfy_baseline(id, binding);
            }
            function.observe_air_conditioner(
                value,
                observation.source_time,
                observation.received_at,
            );
        }
        self.property_sources.borrow_mut().insert(
            super::super::observations::key(id, binding),
            observation.source,
        );
    }
    pub(in crate::xiaomi::adapter) fn decode_air_conditioner(
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
    ) -> Result<crate::device::AirConditionerValue, crate::xiaomi::capability::CapabilityError>
    {
        use crate::{device::AirConditionerValue, xiaomi::capability::CapabilityError};
        let ac = function
            .air_conditioner()
            .expect("air-conditioner function");
        if binding == ac.power() {
            Ok(AirConditionerValue::Power(value.as_bool().ok_or(
                CapabilityError::InvalidValue("air-conditioner power"),
            )?))
        } else if binding == ac.mode().property() {
            Ok(AirConditionerValue::Mode(ac.mode().read_value(value)?))
        } else if binding == ac.temperature().property() {
            Ok(AirConditionerValue::Temperature(
                ac.temperature().read_value(value)?,
            ))
        } else if binding == ac.speed().property() {
            Ok(AirConditionerValue::Speed(ac.speed().read_value(value)?))
        } else {
            debug_assert_eq!(Some(binding), ac.swing());
            Ok(AirConditionerValue::Swing(value.as_bool().ok_or(
                CapabilityError::InvalidValue("air-conditioner swing"),
            )?))
        }
    }
    pub(in crate::xiaomi::adapter) fn observe_fan(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: FanObservation,
        observation: Observation,
        origin: Origin,
    ) {
        if self.life.is_closed() {
            return;
        }
        let id = function.id();
        if !self.observations.permits(id, binding, origin) {
            return;
        }
        self.observations.observed(self, id, binding, origin);
        if matches!(origin, Origin::Notification) {
            self.refresh.borrow_mut().satisfy_baseline(id, binding);
        }
        match value {
            FanObservation::Mode(value) => {
                self.fan_modes.borrow_mut().insert(
                    id.to_owned(),
                    super::super::model::NativeModeObservation {
                        value: Some(value),
                        source_time: observation.source_time,
                        received_at: Some(observation.received_at),
                    },
                );
            }
            FanObservation::Value(value) => {
                let function = self.physical().function(id).unwrap();
                if matches!(origin, Origin::Cache) {
                    function.observe_cached_fan(
                        value,
                        observation.source_time,
                        observation.received_at,
                    );
                } else {
                    function.observe_fan(value, observation.source_time, observation.received_at);
                }
            }
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
    ) -> Result<FanObservation, crate::xiaomi::capability::CapabilityError> {
        use crate::{device::FanValue, xiaomi::capability::CapabilityError};
        let fan = function.fan().expect("fan function");
        if binding == fan.power() {
            Ok(FanObservation::Value(FanValue::Power(
                value
                    .as_bool()
                    .ok_or(CapabilityError::InvalidValue("fan power"))?,
            )))
        } else if let Some(mode) = fan.mode().filter(|mode| binding == mode.property()) {
            Ok(FanObservation::Mode(mode.read_value(value)?))
        } else if binding == fan.speed().property() {
            Ok(FanObservation::Value(FanValue::Speed(
                fan.speed().read_value(value)?,
            )))
        } else {
            debug_assert_eq!(Some(binding), fan.swing());
            Ok(FanObservation::Value(FanValue::Swing(
                value
                    .as_bool()
                    .ok_or(CapabilityError::InvalidValue("fan swing"))?,
            )))
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
        if !self.observations.permits(id, binding, origin) {
            return;
        }
        self.observations.observed(self, id, binding, origin);
        if matches!(origin, Origin::Notification) {
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
    if !work.observations.permits(BATTERY_CHANNEL, binding, origin) {
        return Ok(());
    }
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
    if !matches!(origin, Origin::Cache) {
        work.set_battery_read_problem(None);
    }
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
    let id = function.id();
    if !work.observations.permits(id, binding, origin) {
        return Ok(());
    }
    let measurement = function
        .measurement()
        .expect("caller selected a measurement function");
    let decoded = measurement.read_value(value)?;
    if work.life.is_closed() {
        return Ok(());
    }
    work.observations.observed(work, id, binding, origin);
    work.physical()
        .function(id)
        .expect("installed source function")
        .observe_measurement(decoded, source_time, received_at)
        .map_err(|_| crate::xiaomi::capability::CapabilityError::InvalidValue("measurement"))?;
    Ok(())
}
