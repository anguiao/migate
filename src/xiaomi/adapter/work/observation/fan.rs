use super::*;

impl DeviceWork {
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
                    super::super::super::model::NativeModeObservation {
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
            super::super::super::observations::key(id, binding),
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
}
