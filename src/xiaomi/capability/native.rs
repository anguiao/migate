//! A native device description validated once at the source boundary.

use super::{CapabilityError, ParsedDevice, is_ls2307_type, ls2307, outlet};
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
        if !super::description::Urn::parse(&instance_type).is_ok_and(|urn| urn.entity == "device") {
            return Err(CapabilityError::InvalidDescription("native instance type"));
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
    let cp7pd = model == outlet::CP7PD_MODEL && outlet::is_cp7pd_type(instance_type);
    if let Some(trigger) = device.refresh_trigger()
        && (!cp7pd || trigger != (super::PropertyBinding { siid: 2, piid: 1 }))
    {
        return Err("Xiaomi refresh trigger is unsupported");
    }
    if let Some(metadata) = device.recognition() {
        for patch in metadata.patches() {
            let function = device
                .functions()
                .iter()
                .find(|function| function.id() == patch.target())
                .unwrap();
            match patch.id() {
                id if id == super::patch::CP7PD_CHANNELS.id
                    && patch.stage() == super::patch::CP7PD_CHANNELS.stage
                    || id == super::patch::CP7PD_REFRESH.id
                        && patch.stage() == super::patch::CP7PD_REFRESH.stage =>
                {
                    let expected = match patch.target() {
                        "outlet:3" => 3,
                        "outlet:4" => 4,
                        "outlet:9" => 9,
                        _ => return Err("Xiaomi cp7pd patch target is invalid"),
                    };
                    if !cp7pd
                        || function.kind() != crate::device::FunctionKind::Outlet
                        || function.power()
                            != Some(super::PropertyBinding {
                                siid: expected,
                                piid: 1,
                            })
                    {
                        return Err("Xiaomi cp7pd patch binding is invalid");
                    }
                    if id == super::patch::CP7PD_REFRESH.id && device.refresh_trigger().is_none() {
                        return Err("Xiaomi cp7pd refresh patch has no trigger");
                    }
                }
                id if id == super::patch::LS2307_RGB.id
                    && patch.stage() == super::patch::LS2307_RGB.stage
                    && model == ls2307::MODEL
                    && is_ls2307_type(instance_type)
                    && function
                        .lighting()
                        .is_some_and(|light| light.mode().is_some()) => {}
                id if id == super::patch::DMAKER_STRAIGHT.id
                    && patch.stage() == super::patch::DMAKER_STRAIGHT.stage
                    && model.starts_with("dmaker.")
                    && super::description::Urn::parse(instance_type)
                        .is_ok_and(|urn| urn.vendor.starts_with("dmaker-"))
                    && function.fan().is_some_and(|fan| fan.mode().is_some()) => {}
                _ => return Err("Xiaomi applied patch is invalid"),
            }
        }
    }
    for function in device.functions() {
        if let Some(lighting) = function.lighting()
            && let Some(mode) = lighting.mode()
        {
            let (Some(brightness), Some(temperature), Some(color)) = (
                lighting.brightness(),
                lighting.temperature(),
                lighting.color(),
            ) else {
                return Err("Xiaomi ls2307 adapter is incompatible");
            };
            if model != ls2307::MODEL
                || !is_ls2307_type(instance_type)
                || function.power() != Some(ls2307::POWER)
                || !ls2307::main_bindings(ls2307::POWER, brightness, temperature)
                || !ls2307::is_color(color)
                || !ls2307::is_mode(mode)
            {
                return Err("Xiaomi ls2307 adapter is incompatible");
            }
        }
    }
    let dmaker = super::description::Urn::parse(instance_type)
        .is_ok_and(|urn| urn.vendor.starts_with("dmaker-"))
        && model.starts_with("dmaker.");
    for fan in device
        .functions()
        .iter()
        .filter_map(|function| function.fan())
    {
        if fan.mode().is_some()
            && (!dmaker
                || fan.speed().encoding()
                    != &(super::FanSpeedEncoding::Range {
                        min: 1,
                        max: 100,
                        step: 1,
                    })
                || fan.speed().property().siid == fan.power().siid)
        {
            return Err("Xiaomi fan mode is incompatible with its model or speed binding");
        }
    }
    Ok(())
}
