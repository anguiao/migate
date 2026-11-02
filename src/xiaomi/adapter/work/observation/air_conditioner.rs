use super::*;

impl DeviceWork {
    pub(in crate::xiaomi::adapter) fn mcn04_problem(
        &self,
        id: &str,
        binding: PropertyBinding,
    ) -> Option<String> {
        self.mcn04_problems
            .borrow()
            .get(&super::super::super::observations::key(id, binding))
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
        let key = super::super::super::observations::key(id, binding);
        let result = if binding == patch.state {
            super::super::super::mcn04::state(ac, value).map(|report| {
                physical.observe_air_conditioner_report(
                    &report.values,
                    observation.source_time,
                    observation.received_at,
                );
                report.problem
            })
        } else {
            debug_assert_eq!(binding, patch.range);
            super::super::super::mcn04::range(ac, value).map(|constraints| {
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
            super::super::super::observations::key(id, binding),
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
            self.resolve_initial(id, binding);
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
            super::super::super::observations::key(id, binding),
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
}
