use super::{CommandError, Function, FunctionSnapshot, LightMode, LightPatch};

#[cfg(test)]
mod tests;

/// One validated logical report, bound to the function that will apply it.
pub(crate) struct LightObservation<'a> {
    function: &'a Function,
    patch: LightPatch,
    clear_temperature: bool,
}

impl<'a> LightObservation<'a> {
    fn new(
        function: &'a Function,
        mut patch: LightPatch,
        project_temperature: bool,
    ) -> Result<Self, CommandError> {
        let profile = function
            .description()
            .kind
            .light_profile()
            .ok_or(CommandError::Unsupported)?;
        if patch.brightness_percent.is_some() && !profile.has_brightness() {
            return Err(CommandError::Unsupported);
        }
        if let Some(percent) = patch.brightness_percent
            && (!percent.is_finite() || !(0.0..=100.0).contains(&percent))
        {
            return Err(CommandError::Rejected(
                "brightness is outside 0..100 percent".into(),
            ));
        }
        let mut clear_temperature = false;
        if let Some(value) = patch.temperature_kelvin {
            let interval = profile
                .temperature_interval()
                .ok_or(CommandError::Unsupported)?;
            if !interval.contains(value) {
                if !project_temperature {
                    return Err(CommandError::Rejected(
                        "color temperature is outside its physical range".into(),
                    ));
                }
                patch.temperature_kelvin = None;
                clear_temperature = true;
            }
        }
        if patch.mode == Some(LightMode::Temperature) && profile.temperature_interval().is_none() {
            return Err(CommandError::Unsupported);
        }
        if !profile.has_rgb() && (patch.color.is_some() || patch.mode == Some(LightMode::Rgb)) {
            return Err(CommandError::Unsupported);
        }
        Ok(Self {
            function,
            patch,
            clear_temperature,
        })
    }

    pub(crate) fn patch(&self) -> LightPatch {
        self.patch
    }

    pub(crate) fn apply(self) -> FunctionSnapshot {
        self.function.apply_light_observation(self)
    }

    pub(super) fn into_parts(self) -> (LightPatch, bool) {
        (self.patch, self.clear_temperature)
    }
}

impl Function {
    /// A native value outside a group's effective interval becomes unknown.
    pub(crate) fn project_light_observation(
        &self,
        patch: LightPatch,
    ) -> Result<LightObservation<'_>, CommandError> {
        LightObservation::new(self, patch, true)
    }
}
