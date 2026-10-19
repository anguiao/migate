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
    BrightnessBinding, CapabilityError, ColorBinding, LightBindings, ModeBinding, NumericFormat,
    ParsedDevice, ParsedFunction, PropertyBinding, TemperatureBinding,
};
pub(crate) use outlet::is_outlet_type;
pub(crate) use repair::{
    RepairPlan, effective_temperature_range, narrowed_profile, plan_rgb_repair, repaired_profile,
};

pub(crate) fn is_light_group(key: &crate::device::DeviceKey, model: &str) -> bool {
    key.id.starts_with("group.") || model == "mijia.light.group3"
}

/// Model/URN constraints for one restored device description. Storage uses
/// this same capability boundary instead of re-deriving supported kinds.
pub(crate) fn validate_model_topology(
    device: &ParsedDevice,
    model: &str,
    instance_type: &str,
) -> Result<(), &'static str> {
    if is_light_type(instance_type) {
        let function = &device.functions()[0];
        let Some(lighting) = function.lighting() else {
            return Err("Stored Xiaomi light topology is invalid");
        };
        if model == ls2307::MODEL && !is_ls2307_type(instance_type) {
            return Err("Stored Xiaomi device model is incompatible with its instance type");
        }
        if model == ls2307::MODEL {
            let (Some(brightness), Some(temperature), Some(color), Some(mode)) = (
                lighting.brightness(),
                lighting.temperature(),
                lighting.color(),
                lighting.mode(),
            ) else {
                return Err("Stored Xiaomi ls2307 adapter is incompatible");
            };
            if !ls2307::main_bindings(function.power(), brightness, temperature)
                || !ls2307::is_color(color)
                || !ls2307::is_mode(mode)
            {
                return Err("Stored Xiaomi ls2307 adapter is incompatible");
            }
        }
        return Ok(());
    }
    if is_outlet_type(instance_type) {
        if model == ls2307::MODEL {
            return Err("Stored Xiaomi device model is incompatible with its instance type");
        }
        if device
            .functions()
            .iter()
            .any(|function| function.kind().is_light())
        {
            return Err("Stored Xiaomi outlet description is invalid");
        }
        let cp7pd_type = outlet::is_cp7pd_type(instance_type);
        let cp7pd_model = model == outlet::CP7PD_MODEL;
        if cp7pd_model != cp7pd_type {
            return Err("Stored Xiaomi device model is incompatible with its instance type");
        }
        if cp7pd_model {
            return if outlet::is_cp7pd_topology(device) {
                Ok(())
            } else {
                Err("Stored Xiaomi outlet topology is invalid")
            };
        }
        let single_main = device.functions().len() == 1
            && device.functions()[0].id() == "main"
            && device.refresh_trigger().is_none();
        return if single_main {
            Ok(())
        } else {
            Err("Stored Xiaomi outlet topology is invalid")
        };
    }
    Err("Stored Xiaomi device type is unsupported")
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
pub(crate) mod tests;
