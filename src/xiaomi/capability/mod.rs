mod description;
mod group;
mod light;
pub(crate) mod ls2307;
mod model;
mod outlet;
mod repair;

pub(crate) use group::parse_group;
pub(crate) use light::is_light_type;
pub(crate) use ls2307::is_type as is_ls2307_type;
pub use model::{
    BrightnessConversion, CapabilityError, LightBindings, ModeBinding, NumericBinding,
    NumericFormat, NumericUnit, ParsedDevice, ParsedFunction, PropertyBinding,
};
pub(crate) use outlet::is_outlet_type;
pub(crate) use repair::{
    RepairPlan, effective_temperature_range, narrowed_capabilities, plan_rgb_repair,
    repaired_capabilities,
};

pub(crate) fn is_light_group(key: &crate::device::DeviceKey, model: &str) -> bool {
    key.id.starts_with("group.") || model == "mijia.light.group3"
}

pub fn parse_device(
    key: crate::device::DeviceKey,
    instance: &serde_json::Value,
) -> Result<ParsedDevice, CapabilityError> {
    parse_device_for_model(key, "", instance)
}
pub fn parse_device_for_model(
    key: crate::device::DeviceKey,
    model: &str,
    instance: &serde_json::Value,
) -> Result<ParsedDevice, CapabilityError> {
    // Groups are never classified here: admission comes only from confirmed
    // catalog relations, which call `parse_group` directly.
    let description = description::Description::parse(instance)?;
    if model == ls2307::MODEL
        && !instance
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_ls2307_type)
    {
        return Err(CapabilityError::IncompatibleModel(ls2307::MODEL));
    }
    if description.urn.standard("device", "light", 0x0000a001) {
        light::parse_light(key, model, &description)
    } else {
        outlet::parse_outlet(key, &description)
    }
}

#[cfg(test)]
mod tests;
