pub(in crate::xiaomi::adapter) enum FanObservation {
    Value(crate::device::FanValue),
    Mode(u32),
}

use super::*;

mod air_conditioner;
mod curtain;
mod fan;
mod heater;
mod shared;

pub(super) use air_conditioner::AirConditionerObservations;
pub(super) use fan::FanObservations;
pub(super) use shared::SharedFeedback;

impl DeviceWork {
    fn record_relay_power_source(&self, id: &str, source: PathKind) {
        self.observations
            .record_property_source(id, self.binding(id), source);
        self.observations.record_function_source(id, source);
    }

    pub(in crate::xiaomi::adapter) fn observe_relay_power_notification(
        &self,
        id: &str,
        value: bool,
        source: PathKind,
    ) -> bool {
        if !self
            .observations
            .notify(self, id, value, Origin::Notification)
        {
            return false;
        }
        self.record_relay_power_source(id, source);
        true
    }

    pub(in crate::xiaomi::adapter) fn observe_relay_mode(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
        observation: Observation,
        origin: Origin,
    ) -> Result<(), crate::xiaomi::capability::CapabilityError> {
        use crate::device::RelayMode;
        use crate::xiaomi::capability::CapabilityError;
        let mode = match value.as_u64() {
            Some(0) => RelayMode::Wired,
            Some(1) => RelayMode::Wireless,
            _ => return Err(CapabilityError::InvalidValue("relay mode")),
        };
        if self.life.is_closed() || matches!(origin, Origin::Cache) {
            return Ok(());
        }
        let id = function.id();
        self.observations.observed(self, id, binding, origin);
        if matches!(origin, Origin::Notification) {
            self.refresh.borrow_mut().satisfy_baseline(id, binding);
        }
        self.observations
            .record_property_source(id, binding, observation.source);
        if self
            .physical()
            .function(id)
            .unwrap()
            .observe_relay_mode(mode)
        {
            self.configuration_changed.notify(usize::MAX);
        }
        Ok(())
    }

    pub(in crate::xiaomi::adapter) fn observation_source(&self, id: &str) -> Option<PathKind> {
        self.observations.function_source(id)
    }
    pub(in crate::xiaomi::adapter) fn property_source(
        &self,
        id: &str,
        binding: PropertyBinding,
    ) -> Option<PathKind> {
        self.observations.property_source(id, binding)
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
        self.observations
            .record_function_source(function.id(), observation.source);
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
            .relay_mode()
            .is_some_and(|(mode, _)| mode == binding)
        {
            return self.observe_relay_mode(function, binding, value, observation, Origin::Read);
        }
        if self.is_standard_mode_binding(id, binding) {
            let _ = self.observe_standard_mode(function, binding, value, observation, Origin::Read);
            return Ok(());
        }
        if function
            .air_conditioner()
            .is_some_and(|ac| ac.mcn04().is_some())
        {
            self.observe_mcn04(function, binding, value, observation, Origin::Read)?;
        } else if function.heater().is_some() {
            let decoded = Self::decode_heater(function, binding, value)?;
            self.observe_heater(function, binding, decoded, observation, Origin::Read);
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
            if self.observations.observe_read(self, id, stamp, value)
                && function.relay_mode().is_some()
            {
                self.record_relay_power_source(id, observation.source);
            }
        }
        Ok(())
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
