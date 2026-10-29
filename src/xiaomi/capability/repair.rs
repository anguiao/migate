//! Group capability composition from already adapted light functions.
//!
//! Native group bindings remain unchanged; member RGB uses the same normalized
//! function contracts and execution policies as independent lights.

use super::ParsedFunction;

use crate::device::{FunctionKey, LightProfile, TemperatureInterval};

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
fn effective_temperature_range(
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
    TemperatureInterval::new(min, max).expect("intersected valid temperature intervals")
}

/// One source-owned composition result for a resolved native group light.
#[derive(Debug)]
pub(crate) struct GroupLight {
    pub(crate) profile: LightProfile,
    pub(crate) repair: Option<RepairPlan>,
    pub(crate) reason: Option<String>,
}

pub(crate) fn compose_group_light(
    group: &ParsedFunction,
    members: &[(&FunctionKey, &ParsedFunction)],
    unresolved: Option<&str>,
) -> GroupLight {
    let light = group
        .lighting()
        .expect("a native group has a light function");
    let native = light.profile();
    let profile = match native.temperature_interval() {
        Some(interval) if unresolved.is_none() => {
            let intervals = members
                .iter()
                .map(|(_, member)| {
                    member
                        .lighting()
                        .expect("a resolved member is a light")
                        .temperature()
                        .map(|temperature| temperature.interval())
                })
                .collect::<Option<Vec<_>>>();
            let interval = intervals.as_ref().map_or(interval, |members| {
                effective_temperature_range(interval, members)
            });
            narrowed_profile(native, interval)
        }
        _ => native,
    };
    let plan = if light.color().is_some() {
        Ok(None)
    } else if !matches!(profile, LightProfile::ColorTemperature(_)) {
        Err("native group cannot be extended with RGB".into())
    } else if let Some(reason) = unresolved {
        Err(reason.to_owned())
    } else {
        rgb_members(members).map(Some)
    };
    match plan {
        Ok(Some(repair)) => GroupLight {
            profile: LightProfile::ExtendedColor(profile.temperature_interval().unwrap()),
            repair: Some(repair),
            reason: None,
        },
        Ok(None) => GroupLight {
            profile,
            repair: None,
            reason: None,
        },
        Err(reason) => GroupLight {
            profile,
            repair: None,
            reason: Some(reason),
        },
    }
}

fn rgb_members(members: &[(&FunctionKey, &ParsedFunction)]) -> Result<RepairPlan, String> {
    if members.is_empty() {
        return Err("native group has no effective members".into());
    }
    let mut keys = Vec::with_capacity(members.len());
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
        keys.push((*key).clone());
    }
    Ok(RepairPlan { members: keys })
}

/// Replace only the color-temperature interval of a native-only profile with
/// the narrowed logical interval.
fn narrowed_profile(native: LightProfile, temperature: TemperatureInterval) -> LightProfile {
    match native {
        LightProfile::ColorTemperature(_) => LightProfile::ColorTemperature(temperature),
        LightProfile::ExtendedColor(_) => LightProfile::ExtendedColor(temperature),
        other => other,
    }
}

#[cfg(test)]
#[path = "tests/repair.rs"]
mod tests;
