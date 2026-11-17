use super::{App, EffectiveConfiguration, installed_function};
use crate::{
    configuration::CommittedConfiguration,
    device::{FunctionKey, FunctionKind},
    xiaomi::adapter::publication::SourceEntry,
};
use std::rc::Rc;

impl App {
    /// Persist the relationship before installing it on the serving function.
    pub(crate) fn bind_sensor(
        &self,
        target: FunctionKey,
        source: FunctionKey,
    ) -> Result<bool, String> {
        let current = self.current();
        if current.configuration.temperature_bindings.get(&target) == Some(&source) {
            return Ok(false);
        }
        self.validate_binding_function(&current, &target, true)?;
        self.validate_binding_function(&current, &source, false)?;
        let mut configuration = (*current.configuration).clone();
        configuration.temperature_bindings.insert(target, source);
        self.commit_settings(&current, configuration, |next| {
            activate_temperature_bindings(&next.configuration, &next.entries);
        })
        .map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// Existing relationships can be removed even after either device disappears.
    pub(crate) fn unbind_sensor(&self, target: &FunctionKey) -> Result<bool, String> {
        let current = self.current();
        if !current
            .configuration
            .temperature_bindings
            .contains_key(target)
        {
            let device = current
                .configuration
                .device(&target.device)
                .ok_or("Target device does not exist.")?;
            if !device
                .functions
                .iter()
                .any(|function| function.id == target.id)
            {
                return Err("Target function does not exist.".into());
            }
            if let Some(native) = &device.native {
                let function = native
                    .device()
                    .functions()
                    .iter()
                    .find(|function| function.id() == target.id);
                if !function.is_some_and(|function| {
                    matches!(
                        function.description().kind,
                        FunctionKind::AirConditioner { .. }
                    )
                }) {
                    return Err("Target function is not an air conditioner.".into());
                }
            }
            return Ok(false);
        }
        let mut configuration = (*current.configuration).clone();
        configuration.temperature_bindings.remove(target);
        self.commit_settings(&current, configuration, |next| {
            activate_temperature_bindings(&next.configuration, &next.entries);
        })
        .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn validate_binding_function(
        &self,
        current: &EffectiveConfiguration,
        key: &FunctionKey,
        target: bool,
    ) -> Result<(), String> {
        let label = if target { "Target" } else { "Sensor" };
        if !self
            .source
            .binding_accessible(&current.configuration.scope, &key.device)
        {
            return Err(format!(
                "{label} device is not in the current home or has no current access."
            ));
        }
        let function = installed_function(&current.entries, key)
            .ok_or_else(|| format!("{label} function does not exist."))?;
        let valid = if target {
            matches!(
                function.description().kind,
                FunctionKind::AirConditioner { .. }
            )
        } else {
            matches!(function.description().kind, FunctionKind::Temperature(_))
        };
        if !valid {
            return Err(if target {
                "Target function is not an air conditioner."
            } else {
                "Sensor function is not an environmental temperature function."
            }
            .into());
        }
        Ok(())
    }
}

/// Reconcile relationships only after committed sources have been installed.
pub(super) fn activate_temperature_bindings(
    configuration: &CommittedConfiguration,
    entries: &[Rc<SourceEntry>],
) {
    for entry in entries {
        for function in entry.physical.functions() {
            if matches!(
                function.description().kind,
                FunctionKind::AirConditioner { .. }
            ) {
                let source = configuration
                    .temperature_bindings
                    .get(function.function_key())
                    .map(|key| (key.clone(), installed_function(entries, key)));
                function.set_ambient_temperature_source(source);
            }
        }
    }
}
