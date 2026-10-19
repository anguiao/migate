//! Concrete `devcea.light.ls2307` RGB repair planning and the independent
//! effective color-temperature interval rule.
//!
//! Native group capabilities stay exactly as parsed; this module only decides
//! whether every effective member provides the genuine cross-service RGB and
//! color-mode bindings, and computes the logical color-temperature interval
//! shared with the native group. No group RGB MIoT binding is invented and the
//! genuine native binding grid stays untouched.

use super::{ParsedDevice, ParsedFunction, ls2307};

use crate::device::{DeviceKey, LightProfile, NumericRange, standard_light_profile};

/// The committed RGB repair inputs for one native group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepairPlan {
    /// Effective member identities in the order carried by the relation.
    pub(crate) members: Vec<DeviceKey>,
}

/// The logical published color-temperature interval.
///
/// Uses the intersection of every explicit member range with the native
/// declaration when that is a valid strict narrowing; otherwise the native
/// declared interval. The logical interval is independent of the genuine
/// native quantization grid.
pub(crate) fn effective_temperature_range(
    native: NumericRange,
    members: &[NumericRange],
) -> NumericRange {
    if members.is_empty() {
        return native;
    }
    let mut min = native.min;
    let mut max = native.max;
    for range in members {
        min = min.max(range.min);
        max = max.min(range.max);
    }
    if min >= max || (min == native.min && max == native.max) {
        return native;
    }
    let logical = NumericRange { min, max, step: 1 };
    if logical.mired_bounds().is_ok() {
        logical
    } else {
        native
    }
}

/// Plan RGB repair for a native group whose members are all genuine ls2307
/// adapters. `Ok(None)` means the native group already provides RGB; `Err`
/// carries the reason the native group stays unrepaired.
pub(crate) fn plan_rgb_repair<'a>(
    group: &'a ParsedDevice,
    members: impl IntoIterator<Item = (&'a str, &'a str, &'a str, &'a ParsedDevice)>,
) -> Result<Option<RepairPlan>, String> {
    let function = group
        .functions
        .first()
        .ok_or_else(|| "native group has no light function".to_string())?;
    let light = function
        .lighting
        .as_ref()
        .ok_or_else(|| "native group is not a light".to_string())?;
    if light.color.is_some() {
        return Ok(None);
    }
    if light.mode.is_some() {
        return Err("native group mode is not an RGB binding".into());
    }
    if standard_light_profile(&function.description.capabilities)
        != Ok(LightProfile::ColorTemperature)
    {
        return Err("native group is not a color-temperature light".into());
    }
    if light.temperature.is_none() {
        return Err("native group has no color temperature".to_string());
    }
    let mut keys = Vec::new();
    for (did, model, instance_type, device) in members {
        let member_function = device
            .functions
            .first()
            .ok_or_else(|| format!("member {did} is not a supported light"))?;
        let member = member_function
            .lighting
            .as_ref()
            .ok_or_else(|| format!("member {did} is not a supported light"))?;
        if model != ls2307::MODEL || !ls2307::is_type(instance_type) {
            return Err(format!("member {did} is not the ls2307 adapter"));
        }
        if member_function.light_profile() != Ok(LightProfile::ExtendedColor) {
            return Err(format!("member {did} is not an extended color light"));
        }
        let (Some(color), Some(mode)) = (&member.color, &member.mode) else {
            return Err(format!(
                "member {did} lacks genuine RGB and color-mode bindings"
            ));
        };
        if *color != ls2307::COLOR {
            return Err(format!("member {did} has an incompatible RGB binding"));
        }
        if *mode != ls2307::MODE {
            return Err(format!(
                "member {did} has an incompatible color mode binding"
            ));
        }
        keys.push(DeviceKey::new("xiaomi", did));
    }
    if keys.is_empty() {
        return Err("native group has no effective members".into());
    }
    Ok(Some(RepairPlan { members: keys }))
}

/// The published function capabilities after repair: native on/off,
/// brightness and color temperature with the logical interval, plus RGB.
pub(crate) fn repaired_capabilities(
    function: &ParsedFunction,
    temperature: NumericRange,
) -> Vec<crate::device::Capability> {
    function
        .description
        .capabilities
        .iter()
        .filter(|capability| !matches!(capability, crate::device::Capability::ColorTemperature(_)))
        .cloned()
        .chain([
            crate::device::Capability::ColorTemperature(temperature),
            crate::device::Capability::ColorRgb,
        ])
        .collect()
}

/// Replace only the color-temperature capability of a native-only profile
/// with the narrowed logical interval.
pub(crate) fn narrowed_capabilities(
    function: &ParsedFunction,
    temperature: NumericRange,
) -> Vec<crate::device::Capability> {
    function
        .description
        .capabilities
        .iter()
        .map(|capability| match capability {
            crate::device::Capability::ColorTemperature(_) => {
                crate::device::Capability::ColorTemperature(temperature)
            }
            capability => *capability,
        })
        .collect()
}
