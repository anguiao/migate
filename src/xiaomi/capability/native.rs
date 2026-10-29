//! A native device description validated once at the source boundary.

use super::{
    CapabilityError, ParsedDevice, fan, is_curtain_type, is_fan_type, is_light_type,
    is_ls2307_type, is_outlet_type, ls2307, outlet,
};
use crate::device::PhysicalDeviceDescription;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDescription {
    model: String,
    instance_type: String,
    device: ParsedDevice,
}

impl NativeDescription {
    pub(crate) fn new(
        model: String,
        instance_type: String,
        device: ParsedDevice,
    ) -> Result<Self, CapabilityError> {
        if [&model, &instance_type]
            .iter()
            .any(|value| value.is_empty() || value.chars().any(char::is_control))
        {
            return Err(CapabilityError::InvalidDescription(
                "native model or instance type",
            ));
        }
        validate_model_topology(&device, &model, &instance_type)
            .map_err(CapabilityError::InvalidDescription)?;
        Ok(Self {
            model,
            instance_type,
            device,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    pub(crate) fn instance_type(&self) -> &str {
        &self.instance_type
    }
    pub(crate) fn device(&self) -> &ParsedDevice {
        &self.device
    }

    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        self.device.physical_description(label)
    }
}

/// Model/URN constraints shared by network descriptions and stored bindings.
fn validate_model_topology(
    device: &ParsedDevice,
    model: &str,
    instance_type: &str,
) -> Result<(), &'static str> {
    let cp7pd_type = outlet::is_cp7pd_type(instance_type);
    let cp7pd_model = model == outlet::CP7PD_MODEL;
    if cp7pd_model || cp7pd_type {
        if cp7pd_model != cp7pd_type {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        if !outlet::is_cp7pd_topology(device) {
            return Err("Xiaomi outlet topology is invalid");
        }
    } else if device.refresh_trigger().is_some() {
        return Err("Xiaomi refresh trigger is unsupported");
    }

    if model == ls2307::MODEL {
        if !is_ls2307_type(instance_type) {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        let function = device
            .functions()
            .iter()
            .find(|function| {
                function.power() == Some(ls2307::POWER) && function.lighting().is_some()
            })
            .ok_or("Xiaomi ls2307 adapter is incompatible")?;
        let lighting = function.lighting().unwrap();
        let (Some(brightness), Some(temperature), Some(color), Some(mode)) = (
            lighting.brightness(),
            lighting.temperature(),
            lighting.color(),
            lighting.mode(),
        ) else {
            return Err("Xiaomi ls2307 adapter is incompatible");
        };
        if !ls2307::main_bindings(ls2307::POWER, brightness, temperature)
            || !ls2307::is_color(color)
            || !ls2307::is_mode(mode)
        {
            return Err("Xiaomi ls2307 adapter is incompatible");
        }
    }

    let dmaker = fan::is_dmaker_fan_type(instance_type) && model.starts_with("dmaker.");
    if device.functions().iter().any(|function| {
        function
            .fan()
            .is_some_and(|fan| fan.mode().is_some() && !dmaker)
    }) {
        return Err("Xiaomi fan mode is incompatible with its model");
    }
    if device
        .functions()
        .iter()
        .all(|function| !function.kind().is_control())
        || is_fan_type(instance_type)
        || is_curtain_type(instance_type)
        || is_light_type(instance_type)
        || is_outlet_type(instance_type)
    {
        Ok(())
    } else {
        Err("Xiaomi device type is unsupported")
    }
}
