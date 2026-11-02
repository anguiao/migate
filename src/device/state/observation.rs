use super::{
    Function, FunctionSnapshot, MeasurementSnapshot, Power, RuntimeState, RuntimeValue,
    TemperatureSourceState,
};

impl Function {
    /// Update the function's acquisition eligibility without changing its last reading.
    pub(crate) fn set_temperature_source_available(&self, available: bool) {
        let mut state = self.0.state.lock().unwrap();
        let measurement = state.measurement_mut().expect("temperature function");
        let source = measurement
            .temperature_source
            .as_mut()
            .expect("temperature function");
        let next = if available {
            match *source {
                TemperatureSourceState::Unavailable => TemperatureSourceState::AwaitingReading,
                current => current,
            }
        } else {
            TemperatureSourceState::Unavailable
        };
        if *source != next {
            *source = next;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply one validated measurement. Missing source time stays unknown for
    /// this observation instead of inheriting an earlier timestamp.
    pub fn observe_measurement(
        &self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> Result<FunctionSnapshot, crate::device::CommandError> {
        if !self.description().is_measurement() {
            return Err(crate::device::CommandError::Unsupported);
        }
        let observed = MeasurementSnapshot {
            value: Some(value),
            source_time,
            received_at: Some(received_at),
        };
        let mut state = self.0.state.lock().unwrap();
        let current = state
            .measurement_mut()
            .expect("measurement function has measurement state");
        let next_source = match current.temperature_source {
            Some(
                TemperatureSourceState::AwaitingReading | TemperatureSourceState::Available(_),
            ) => Some(TemperatureSourceState::Available(observed)),
            other => other,
        };
        if current.snapshot != observed || current.temperature_source != next_source {
            current.snapshot = observed;
            current.temperature_source = next_source;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub(crate) fn matter_curtain(&self) -> (crate::device::CurtainSnapshot, Option<u32>) {
        let state = self.0.state.lock().unwrap();
        let RuntimeValue::Curtain(curtain) = &state.value else {
            unreachable!("curtain function has curtain state")
        };
        (
            curtain.snapshot,
            curtain.projection.target(curtain.snapshot),
        )
    }
    pub(in crate::device) fn notify_curtain_projection(&self, changed: bool) {
        if changed {
            self.0.changed.notify(usize::MAX);
        }
    }
    /// A replacement source may have missed movement after the previous target.
    pub(crate) fn curtain_source_changed(&self) {
        let mut state = self.0.state.lock().unwrap();
        let changed = state.update_curtain_projection(|p| p.clear_pending_target());
        self.notify_curtain_projection(changed);
    }
    /// Apply one validated curtain report without conflating the three inputs.
    pub(crate) fn observe_curtain(
        &self,
        value: crate::device::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_curtain(value, source_time, received_at, true);
    }
    /// Restore native fields without treating a cached target as a new command.
    pub(crate) fn read_curtain(
        &self,
        value: crate::device::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_curtain(value, source_time, received_at, false);
    }
    fn apply_curtain(
        &self,
        value: crate::device::CurtainValue,
        source_time: Option<i64>,
        received_at: i64,
        notification: bool,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let RuntimeValue::Curtain(curtain) = &mut state.value else {
            unreachable!("curtain function has curtain state")
        };
        let before_target = curtain.projection.target(curtain.snapshot);
        curtain
            .projection
            .observe(&value, curtain.snapshot, notification);
        let snapshot = &mut curtain.snapshot;
        let changed =
            match value {
                crate::device::CurtainValue::CurrentPosition(value) => snapshot
                    .current_position
                    .update(value, source_time, received_at),
                crate::device::CurtainValue::TargetPosition(value) => snapshot
                    .target_position
                    .update(value, source_time, received_at),
                crate::device::CurtainValue::Status(value) => {
                    snapshot.status.update(value, source_time, received_at)
                }
            };
        let projection_changed = before_target != curtain.projection.target(curtain.snapshot);
        if changed || projection_changed {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply one validated native fan property without conflating its fields.
    pub(crate) fn observe_fan(
        &self,
        value: crate::device::FanValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_fan_observation(value, source_time, received_at, true);
    }
    pub(crate) fn observe_air_conditioner(
        &self,
        value: crate::device::AirConditionerValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_air_conditioner_observations(&[value], source_time, received_at, true);
    }
    pub(crate) fn observe_air_conditioner_report(
        &self,
        values: &[crate::device::AirConditionerValue],
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_air_conditioner_observations(values, source_time, received_at, true);
    }
    pub(crate) fn air_conditioner_source_changed(&self) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state
            .air_conditioner_mut()
            .expect("air-conditioner function");
        runtime.planning_mode = None;
        runtime.observed_mode = None;
        if runtime.dynamic_constraints {
            runtime.constraints_current = false;
        }
        runtime.mode_revision = runtime.mode_revision.wrapping_add(1);
        runtime.commands.power_off_sent();
    }
    pub(crate) fn observe_cached_air_conditioner(
        &self,
        value: crate::device::AirConditionerValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_air_conditioner_observations(&[value], source_time, received_at, false);
    }
    fn apply_air_conditioner_observations(
        &self,
        values: &[crate::device::AirConditionerValue],
        source_time: Option<i64>,
        received_at: i64,
        fresh: bool,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let runtime = state
            .air_conditioner_mut()
            .expect("air-conditioner function");
        let mut changed = false;
        for &value in values {
            if let crate::device::AirConditionerValue::Power(power) = value {
                runtime.commands.observe_power(power, fresh);
            }
            if let crate::device::AirConditionerValue::Mode(mode) = value
                && fresh
            {
                runtime.mode_revision = runtime.mode_revision.wrapping_add(1);
                runtime.planning_mode = None;
                runtime.observed_mode = Some(mode);
            }
            changed |= runtime.snapshot.update(value, source_time, received_at);
        }
        if changed {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    pub(crate) fn observe_cached_fan(
        &self,
        value: crate::device::FanValue,
        source_time: Option<i64>,
        received_at: i64,
    ) {
        self.apply_fan_observation(value, source_time, received_at, false);
    }
    fn apply_fan_observation(
        &self,
        value: crate::device::FanValue,
        source_time: Option<i64>,
        received_at: i64,
        fresh: bool,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let RuntimeValue::Fan(fan) = &mut state.value else {
            unreachable!("fan function has fan state")
        };
        let changed = match value {
            crate::device::FanValue::Power(value) => {
                fan.commands.observe_power(value, fresh);
                fan.snapshot.power.update(value, source_time, received_at)
            }
            crate::device::FanValue::Speed(value) => {
                fan.snapshot.speed.update(value, source_time, received_at)
            }
            crate::device::FanValue::Swing(value) => {
                fan.snapshot.swing.update(value, source_time, received_at)
            }
        };
        if changed {
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply one state observation already interpreted at the source boundary.
    pub fn observe_state(
        &self,
        active: Option<bool>,
        input: crate::device::StateInput,
        source_time: Option<i64>,
        received_at: i64,
    ) -> Result<FunctionSnapshot, crate::device::CommandError> {
        let mut state = self.0.state.lock().unwrap();
        let RuntimeValue::StateSensor(current) = &mut state.value else {
            return Err(crate::device::CommandError::Unsupported);
        };
        let observed = crate::device::StateSnapshot {
            active,
            input: Some(input),
            source_time,
            received_at: Some(received_at),
        };
        if *current != observed {
            *current = observed;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        Ok(self.snapshot_locked(&state))
    }
    pub fn observe(&self, power: bool) {
        if !matches!(
            self.description().kind,
            crate::device::FunctionKind::Outlet | crate::device::FunctionKind::Light(_)
        ) {
            return;
        }
        let mut state = self.0.state.lock().unwrap();
        self.update_power(&mut state, power);
    }
    pub(in crate::device) fn update_power(&self, state: &mut RuntimeState, power: bool) {
        let power = power.into();
        if state.power() != Some(power) {
            state.set_power(power);
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Apply a complete logical observation without repeating its admission checks.
    pub(in crate::device) fn apply_light_observation(
        &self,
        observation: crate::device::light_observation::LightObservation<'_>,
    ) -> FunctionSnapshot {
        let (patch, clear_temperature) = observation.into_parts();
        let mut state = self.0.state.lock().unwrap();
        let (before_power, Some(before_lighting)) =
            state.control_parts().expect("light has control state")
        else {
            unreachable!("light has lighting state")
        };
        let mut power = before_power;
        if let Some(value) = patch.power {
            power = value.into();
        }
        let mut lighting = before_lighting;
        if clear_temperature {
            lighting.temperature_kelvin = None;
        }
        if let Some(value) = patch.brightness_percent {
            lighting.brightness_percent = Some(value);
        }
        if let Some(value) = patch.temperature_kelvin {
            lighting.temperature_kelvin = Some(value);
        }
        if let Some(value) = patch.color {
            lighting.color = Some(value);
        }
        if let Some(value) = patch.mode {
            lighting.mode = Some(value);
        }
        if (before_power, Some(before_lighting)) != (power, Some(lighting)) {
            state.set_control_parts(power, Some(lighting));
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
        self.snapshot_locked(&state)
    }
    /// Replace the represented RGB and mode, including unknown fields.
    pub(crate) fn replace_light_color(
        &self,
        color: Option<crate::device::Rgb>,
        mode: Option<crate::device::LightMode>,
    ) {
        let mut state = self.0.state.lock().unwrap();
        let light = state.lighting_mut().expect("light has lighting state");
        if (light.color, light.mode) != (color, mode) {
            light.color = color;
            light.mode = mode;
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
    /// Replace a derived light snapshot, including fields that became unknown.
    pub(crate) fn replace_light_state(&self, power: Power, lighting: crate::device::LightSnapshot) {
        let mut state = self.0.state.lock().unwrap();
        if state.control_parts() != Some((power, Some(lighting))) {
            state.set_control_parts(power, Some(lighting));
            state.revision = state.revision.wrapping_add(1);
            self.0.changed.notify(usize::MAX);
        }
    }
}
