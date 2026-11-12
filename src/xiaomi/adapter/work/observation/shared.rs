use super::*;
use crate::xiaomi::{
    capability::{ReadInput, SHARED_FEEDBACK_CHANNEL},
    legacy_miio::{SharedFeedbackReading, decode_shared_feedback},
};

impl DeviceWork {
    /// Apply one shared legacy response as one group observation. The two fan
    /// levels and the built-in temperature share the same received time, while
    /// each field keeps its own raw value and explicit unknown reason.
    pub(in crate::xiaomi::adapter) fn observe_shared_feedback(
        &self,
        raw: &serde_json::Value,
        stamp: &super::super::super::observations::ReadStamp,
        observation: Observation,
    ) -> super::super::super::refresh::ReadResult {
        use super::super::super::refresh::ReadResult;
        if self.life.is_closed()
            || !self.observations.read_is_current_input(
                self,
                SHARED_FEEDBACK_CHANNEL,
                ReadInput::SharedFeedback,
                stamp,
            )
        {
            return ReadResult::Retired;
        }
        let Ok(reading) = decode_shared_feedback(raw) else {
            return ReadResult::Failed(format!(
                "shared feedback response has an invalid shape: {raw}"
            ));
        };
        self.apply_shared_feedback(&reading, observation);
        ReadResult::Observed
    }

    fn apply_shared_feedback(&self, reading: &SharedFeedbackReading, observation: Observation) {
        let plan = self
            .native
            .plan()
            .shared_feedback()
            .expect("shared feedback is applied only to a planned device");
        self.observations.observed_input(
            self,
            SHARED_FEEDBACK_CHANNEL,
            ReadInput::SharedFeedback,
            Origin::Read,
        );
        {
            let mut state = self.shared_feedback.borrow_mut();
            state.pending = false;
            state.source = Some(observation.source);
            state.native_mode = Some(native_value(&reading.native_mode, observation));
            state.fan_speed_index = Some(super::super::super::model::NativeValueObservation {
                value: reading.fan_speed.raw.clone(),
                source_time: observation.source_time,
                received_at: observation.received_at,
                reason: match (
                    &reading.fan_speed.blow.reason,
                    &reading.fan_speed.ventilation.reason,
                ) {
                    (None, None) => None,
                    (Some(reason), None) | (None, Some(reason)) => Some(reason.clone()),
                    (Some(blow), Some(ventilation)) => {
                        Some(format!("blow: {blow}; ventilation: {ventilation}"))
                    }
                },
            });
            state.temperature = Some(native_value(&reading.temperature, observation));
            state.fan_reasons.clear();
            for (channel, bit) in [
                (
                    crate::xiaomi::capability::SharedFanChannel::Blow,
                    &reading.fan_speed.blow,
                ),
                (
                    crate::xiaomi::capability::SharedFanChannel::Ventilation,
                    &reading.fan_speed.ventilation,
                ),
            ] {
                if let Some(reason) = &bit.reason {
                    state.fan_reasons.insert(channel, reason.clone());
                }
            }
        }

        for fan in &plan.fans {
            let (bit, id) = match fan.channel {
                crate::xiaomi::capability::SharedFanChannel::Blow => {
                    (&reading.fan_speed.blow, &fan.function)
                }
                crate::xiaomi::capability::SharedFanChannel::Ventilation => {
                    (&reading.fan_speed.ventilation, &fan.function)
                }
            };
            let function = self.physical().function(id).unwrap();
            function.observe_fan(
                match bit.level {
                    Some(level) => crate::device::FanValue::Speed(level),
                    None => crate::device::FanValue::SpeedUnknown,
                },
                observation.source_time,
                observation.received_at,
            );
        }

        if let Some(id) = &plan.heater_temperature {
            let heater = self.physical().function(id).unwrap();
            heater.set_heater_temperature_source_available(true);
            match reading.temperature.value {
                Some(value) => {
                    let _ = heater.observe_heater_temperature(
                        value,
                        observation.source_time,
                        observation.received_at,
                    );
                }
                None => {
                    heater.observe_heater_temperature_invalid(
                        observation.source_time,
                        observation.received_at,
                    );
                }
            }
        }
    }
}

fn native_value<T>(
    field: &crate::xiaomi::legacy_miio::SharedField<T>,
    observation: Observation,
) -> super::super::super::model::NativeValueObservation {
    super::super::super::model::NativeValueObservation {
        value: field.raw.clone(),
        source_time: observation.source_time,
        received_at: observation.received_at,
        reason: field.reason.clone(),
    }
}

impl DeviceWork {
    /// Store the real standard mode as a diagnostic-only observation. It never
    /// changes heater/fan power or reinterprets the legacy `bh_mode` string.
    /// Returns whether the recorded value belongs to the instance enumeration.
    pub(in crate::xiaomi::adapter) fn observe_standard_mode(
        &self,
        function: &ParsedFunction,
        binding: crate::xiaomi::capability::PropertyBinding,
        raw: &serde_json::Value,
        observation: Observation,
        origin: Origin,
    ) -> Option<bool> {
        let mode = function.heater()?.diagnostic_mode()?;
        if self.life.is_closed()
            || mode.property != binding
            || !self.observations.permits(function.id(), binding, origin)
        {
            return None;
        }
        self.observations
            .observed(self, function.id(), binding, origin);
        if matches!(origin, Origin::Notification) {
            self.refresh
                .borrow_mut()
                .satisfy_baseline(function.id(), binding);
        }
        let valid = mode.accepts(raw);
        *self.standard_mode.borrow_mut() =
            Some(super::super::super::model::NativeValueObservation {
                value: raw.clone(),
                source_time: observation.source_time,
                received_at: observation.received_at,
                reason: (!valid).then(|| format!("unrecognized standard mode value {raw}")),
            });
        self.property_sources.borrow_mut().insert(
            super::super::super::observations::key(function.id(), binding),
            observation.source,
        );
        Some(valid)
    }
}

impl DeviceWork {
    /// The shared group is owned by the physical device: direct/cloud
    /// availability decides whether the built-in temperature is eligible.
    pub(in crate::xiaomi::adapter) fn reconcile_shared_feedback(&self, context: &Context) {
        let Some(plan) = self.native.plan().shared_feedback() else {
            return;
        };
        let path = context.shared_feedback_read_path(&self.key.id);
        let changed = {
            let mut previous = self.shared_read_path.borrow_mut();
            let changed = match previous.as_ref() {
                None => true,
                Some(old) => match (old.as_ref(), path.as_ref()) {
                    (Some(old), Some(new)) => !old.same(new),
                    (None, None) => false,
                    _ => true,
                },
            };
            *previous = Some(path.clone());
            changed
        };
        if !changed {
            return;
        }
        let available = path.is_some();
        if let Some(id) = &plan.heater_temperature {
            self.physical()
                .function(id)
                .expect("planned heater exists")
                .set_heater_temperature_source_available(available);
        }
        super::super::super::refresh::reconcile_input(
            self,
            context,
            SHARED_FEEDBACK_CHANNEL,
            ReadInput::SharedFeedback,
            available,
        );
    }
}
