use super::*;

impl Function {
    /// Fixtures admit explicit reports through the logical observation constructor.
    pub(crate) fn observe_light(
        &self,
        patch: LightPatch,
    ) -> Result<FunctionSnapshot, CommandError> {
        Ok(LightObservation::new(self, patch, false)?.apply())
    }
}
