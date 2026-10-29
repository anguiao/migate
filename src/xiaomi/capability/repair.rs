//! Group capability composition from already adapted light functions.
//!
//! Native group bindings remain unchanged; member RGB uses the same normalized
//! function contracts and execution policies as independent lights.

use super::{ParsedDevice, ParsedFunction};

use crate::device::{FunctionKey, FunctionKind, LightProfile, TemperatureInterval};

/// The committed RGB repair inputs for one native group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepairPlan {
    /// Exact effective member functions in the order carried by the relation.
    pub(crate) members: Vec<FunctionKey>,
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

/// Plan RGB enhancement using resolved, adapted member functions.
/// `Ok(None)` means that the native group already provides RGB.
pub(crate) fn plan_rgb_repair<'a>(
    group: &ParsedDevice,
    members: impl IntoIterator<Item = (&'a FunctionKey, &'a ParsedFunction)>,
) -> Result<Option<RepairPlan>, String> {
    let function = group
        .unique_light()
        .ok_or_else(|| "native group light is missing or ambiguous".to_string())?;
    let light = function.lighting().expect("group light has bindings");
    if light.color().is_some() {
        return Ok(None);
    }
    if !matches!(
        function.kind(),
        FunctionKind::Light(LightProfile::ColorTemperature(_))
    ) {
        return Err("native group cannot be extended with RGB".into());
    }
    let mut keys = Vec::new();
    for (key, function) in members {
        if function
            .lighting()
            .expect("resolved member has light bindings")
            .color()
            .is_none()
        {
            return Err(format!(
                "member {} {} lacks the RGB light contract",
                key.device.id, key.id
            ));
        }
        keys.push(key.clone());
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
