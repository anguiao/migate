use super::*;

#[derive(Default)]
pub(in crate::xiaomi::adapter) struct SharedFeedbackState {
    pub(in crate::xiaomi::adapter) pending: bool,
    pub(in crate::xiaomi::adapter) native_mode: Option<NativeValueObservation>,
    pub(in crate::xiaomi::adapter) fan_speed_index: Option<NativeValueObservation>,
    pub(in crate::xiaomi::adapter) temperature: Option<NativeValueObservation>,
    pub(in crate::xiaomi::adapter) source: Option<PathKind>,
    pub(in crate::xiaomi::adapter) fan_reasons:
        BTreeMap<crate::xiaomi::capability::SharedFanChannel, String>,
}

#[derive(Default)]
pub(in crate::xiaomi::adapter::work) struct SharedFeedback {
    state: RefCell<SharedFeedbackState>,
    standard_mode: RefCell<Option<NativeValueObservation>>,
}

impl SharedFeedback {
    pub(in crate::xiaomi::adapter::work) fn await_feedback(&self) {
        self.state.borrow_mut().pending = true;
    }
}

use crate::xiaomi::{
    capability::{ReadInput, SHARED_FEEDBACK_CHANNEL},
    legacy_miio::{SharedFeedbackReading, decode_shared_feedback},
};

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn shared_feedback(
        &self,
    ) -> std::cell::Ref<'_, SharedFeedbackState> {
        self.shared_feedback.state.borrow()
    }
    pub(in crate::xiaomi::adapter) fn standard_mode_value(&self) -> Option<NativeValueObservation> {
        self.shared_feedback.standard_mode.borrow().clone()
    }

    pub(in crate::xiaomi::adapter) fn set_shared_temperature_source_available(
        &self,
        available: bool,
    ) {
        if let Some(id) = self
            .native
            .plan()
            .shared_feedback()
            .and_then(|plan| plan.heater_temperature.as_ref())
        {
            self.physical()
                .function(id)
                .expect("planned heater exists")
                .set_heater_temperature_source_available(available);
        }
    }

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
            let mut state = self.shared_feedback.state.borrow_mut();
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
        *self.shared_feedback.standard_mode.borrow_mut() =
            Some(super::super::super::model::NativeValueObservation {
                value: raw.clone(),
                source_time: observation.source_time,
                received_at: observation.received_at,
                reason: (!valid).then(|| format!("unrecognized standard mode value {raw}")),
            });
        self.observations
            .record_property_source(function.id(), binding, observation.source);
        Some(valid)
    }
}
