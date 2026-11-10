use super::*;

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn observe_heater(
        &self,
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: crate::device::HeaterValue,
        observation: Observation,
        origin: Origin,
    ) -> bool {
        if self.life.is_closed() || !self.observations.permits(function.id(), binding, origin) {
            return false;
        }
        let id = function.id();
        let physical = self.physical().function(id).unwrap();
        if matches!(origin, Origin::Cache) {
            physical.observe_cached_heater(value, observation.source_time, observation.received_at);
            self.resolve_initial(id, binding);
        } else {
            self.observations.observed(self, id, binding, origin);
            if matches!(origin, Origin::Notification) {
                self.refresh.borrow_mut().satisfy_baseline(id, binding);
            }
            physical.observe_heater(value, observation.source_time, observation.received_at);
        }
        self.property_sources.borrow_mut().insert(
            super::super::super::observations::key(id, binding),
            observation.source,
        );
        true
    }

    pub(in crate::xiaomi::adapter) fn decode_heater(
        function: &ParsedFunction,
        binding: PropertyBinding,
        value: &serde_json::Value,
    ) -> Result<crate::device::HeaterValue, crate::xiaomi::capability::CapabilityError> {
        use crate::{device::HeaterValue, xiaomi::capability::CapabilityError};
        let heater = function.heater().expect("heater function");
        if binding == heater.power() {
            Ok(HeaterValue::Power(
                value
                    .as_bool()
                    .ok_or(CapabilityError::InvalidValue("heater power"))?,
            ))
        } else {
            debug_assert_eq!(binding, heater.temperature().property());
            Ok(HeaterValue::Temperature(
                heater.temperature().read_value(value)?,
            ))
        }
    }
}
