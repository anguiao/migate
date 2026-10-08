mod description;
mod light;
mod model;
mod outlet;

pub(crate) use light::{is_light_type, is_ls2307_type};
pub use model::{
    BrightnessConversion, CapabilityError, LightBindings, ModeBinding, NumericBinding,
    NumericFormat, NumericUnit, ParsedDevice, ParsedFunction, PropertyBinding,
};
pub(crate) use outlet::is_outlet_type;

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
    let description = description::Description::parse(instance)?;
    if model == "devcea.light.ls2307"
        && !instance
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_ls2307_type)
    {
        return Err(CapabilityError::IncompatibleModel("devcea.light.ls2307"));
    }
    if description.urn.standard("device", "light", 0x0000a001) {
        light::parse_light(key, model, &description)
    } else {
        outlet::parse_outlet(key, &description)
    }
}

#[cfg(test)]
mod tests;
