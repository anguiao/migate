mod description;
mod group;
mod light;
pub(crate) mod ls2307;
mod model;
mod native;
mod outlet;
mod repair;
mod sensor;
mod state_sensor;
pub(crate) use group::parse_group;
pub(crate) use light::is_light_type;
pub(crate) use ls2307::is_type as is_ls2307_type;
pub use model::{
    BatteryBinding, BrightnessBinding, CapabilityError, ColorBinding, LightBindings,
    MeasurementBinding, MeasurementUnit, ModeBinding, NumericFormat, ParsedDevice, ParsedFunction,
    PropertyBinding, TemperatureBinding,
};
pub(crate) use native::NativeDescription;
pub(crate) use outlet::is_outlet_type;
pub(crate) use repair::{
    RepairPlan, effective_temperature_range, narrowed_profile, plan_rgb_repair, repaired_profile,
};
pub use state_sensor::{
    EventArgument, EventBinding, MotionBinding, StateBinding, StateMapping, StateValue,
};

pub(crate) fn is_light_group(key: &crate::device::DeviceKey, model: &str) -> bool {
    key.id.starts_with("group.") || model == "mijia.light.group3"
}

pub fn parse_device_for_model(
    key: crate::device::DeviceKey,
    model: &str,
    instance: &serde_json::Value,
) -> Result<ParsedDevice, CapabilityError> {
    // Groups are never classified here: admission comes only from confirmed
    // catalog relations, which call `parse_group` directly.
    let description = description::Description::parse(instance)?;
    let control = if model == ls2307::MODEL && !is_ls2307_type(instance["type"].as_str().unwrap()) {
        Err(CapabilityError::IncompatibleModel(ls2307::MODEL))
    } else if description.urn.standard("device", "light", 0x0000a001) {
        light::parse_light(key.clone(), model, &description)
    } else {
        outlet::parse_outlet(key.clone(), &description)
    };
    let mut sensors = sensor::measurements(&description);
    sensors.extend(state_sensor::functions(&description));
    if sensors.is_empty() {
        return control;
    }
    let (mut functions, refresh_trigger) = match control {
        Ok(control) => (control.functions().to_vec(), control.refresh_trigger()),
        Err(_) => (Vec::new(), None),
    };
    functions.extend(sensors);
    let (battery, battery_issue) = sensor::battery(&description);
    ParsedDevice::new_with_battery_diagnostic(
        key,
        functions,
        refresh_trigger,
        battery,
        battery_issue,
    )
}

#[cfg(test)]
pub(crate) mod tests;
