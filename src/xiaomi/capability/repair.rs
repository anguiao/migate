//! Concrete `devcea.light.ls2307` RGB repair planning and the independent
//! effective color-temperature interval rule.
//!
//! Native group capabilities stay exactly as parsed; this module only decides
//! whether every effective member provides the genuine cross-service RGB and
//! color-mode bindings, and computes the logical color-temperature interval
//! shared with the native group. No group RGB MIoT binding is invented and the
//! genuine native binding grid stays untouched.

use super::{NativeDescription, ParsedDevice, ls2307};

use crate::device::{DeviceKey, FunctionKind, LightProfile, TemperatureInterval};

/// The committed RGB repair inputs for one native group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepairPlan {
    /// Effective member identities in the order carried by the relation.
    pub(crate) members: Vec<DeviceKey>,
}

/// The logical published color-temperature interval.
///
/// Uses the intersection of every explicit member interval with the native
/// declaration when that is a valid strict narrowing; otherwise the native
/// declared interval. The logical interval is independent of the genuine
/// native quantization grid.
pub(crate) fn effective_temperature_range(
    native: TemperatureInterval,
    members: &[TemperatureInterval],
) -> TemperatureInterval {
    if members.is_empty() {
        return native;
    }
    let mut min = native.min();
    let mut max = native.max();
    for member in members {
        min = min.max(member.min());
        max = max.min(member.max());
    }
    if min >= max || (min == native.min() && max == native.max()) {
        return native;
    }
    TemperatureInterval::new(min, max).unwrap_or(native)
}

/// Plan RGB repair for a native group whose members are all genuine ls2307
/// adapters. `Ok(None)` means the native group already provides RGB; `Err`
/// carries the reason the native group stays unrepaired.
pub(crate) fn plan_rgb_repair<'a>(
    group: &'a ParsedDevice,
    members: impl IntoIterator<Item = &'a NativeDescription>,
) -> Result<Option<RepairPlan>, String> {
    let function = &group.functions()[0];
    let light = function
        .lighting()
        .ok_or_else(|| "native group is not a light".to_string())?;
    if light.color().is_some() {
        return Ok(None);
    }
    if !matches!(
        function.kind(),
        FunctionKind::Light(LightProfile::ColorTemperature(_))
    ) {
        return Err("native group is not a color-temperature light".into());
    }
    let mut keys = Vec::new();
    for member in members {
        if member.model() != ls2307::MODEL {
            return Err(format!(
                "member {} is not the ls2307 adapter",
                member.device().key().id
            ));
        }
        keys.push(member.device().key().clone());
    }
    if keys.is_empty() {
        return Err("native group has no effective members".into());
    }
    Ok(Some(RepairPlan { members: keys }))
}

/// The published profile after repair: native on/off, brightness and color
/// temperature with the logical interval, plus RGB.
pub(crate) fn repaired_profile(temperature: TemperatureInterval) -> LightProfile {
    LightProfile::ExtendedColor(temperature)
}

/// Replace only the color-temperature interval of a native-only profile with
/// the narrowed logical interval.
pub(crate) fn narrowed_profile(
    native: LightProfile,
    temperature: TemperatureInterval,
) -> LightProfile {
    match native {
        LightProfile::ColorTemperature(_) => LightProfile::ColorTemperature(temperature),
        LightProfile::ExtendedColor(_) => LightProfile::ExtendedColor(temperature),
        other => other,
    }
}
