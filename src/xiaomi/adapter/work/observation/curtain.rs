use super::*;

impl DeviceWork {
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
            super::super::super::observations::key(id, binding),
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
