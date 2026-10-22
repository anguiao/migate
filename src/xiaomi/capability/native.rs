//! A native device description validated once at the source boundary.

use super::{
    CapabilityError, ParsedDevice, is_light_type, is_ls2307_type, is_outlet_type,
    is_temperature_humidity_sensor_type, ls2307, outlet,
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
    if device.battery_issue().is_some() && !is_temperature_humidity_sensor_type(instance_type) {
        return Err("Xiaomi battery diagnostic is invalid");
    }
    if is_temperature_humidity_sensor_type(instance_type) {
        if model == ls2307::MODEL || model == outlet::CP7PD_MODEL {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        if device.functions().len() != 2 || !device.is_read_only() {
            return Err("Xiaomi sensor topology is invalid");
        }
        return Ok(());
    }
    if is_light_type(instance_type) {
        let function = &device.functions()[0];
        let Some(lighting) = function.lighting() else {
            return Err("Xiaomi light topology is invalid");
        };
        if model == ls2307::MODEL && !is_ls2307_type(instance_type) {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        if model == ls2307::MODEL {
            let (Some(brightness), Some(temperature), Some(color), Some(mode)) = (
                lighting.brightness(),
                lighting.temperature(),
                lighting.color(),
                lighting.mode(),
            ) else {
                return Err("Xiaomi ls2307 adapter is incompatible");
            };
            if !ls2307::main_bindings(
                function.power().expect("light functions have power"),
                brightness,
                temperature,
            ) || !ls2307::is_color(color)
                || !ls2307::is_mode(mode)
            {
                return Err("Xiaomi ls2307 adapter is incompatible");
            }
        }
        return Ok(());
    }
    if is_outlet_type(instance_type) {
        if model == ls2307::MODEL {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        if device
            .functions()
            .iter()
            .any(|function| function.kind().is_light())
        {
            return Err("Xiaomi outlet description is invalid");
        }
        let cp7pd_type = outlet::is_cp7pd_type(instance_type);
        let cp7pd_model = model == outlet::CP7PD_MODEL;
        if cp7pd_model != cp7pd_type {
            return Err("Xiaomi device model is incompatible with its instance type");
        }
        if cp7pd_model {
            return if outlet::is_cp7pd_topology(device) {
                Ok(())
            } else {
                Err("Xiaomi outlet topology is invalid")
            };
        }
        let single_main = device.functions().len() == 1
            && device.functions()[0].id() == "main"
            && device.refresh_trigger().is_none();
        return if single_main {
            Ok(())
        } else {
            Err("Xiaomi outlet topology is invalid")
        };
    }
    Err("Xiaomi device type is unsupported")
}
