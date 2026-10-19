//! Pure aggregation rules for a light group's reported state.
//!
//! Callers provide only current valid member snapshots and the group's
//! confirmed member count. Representative selection and aggregation stay free
//! of source lifecycles, storage, and protocol wiring.

use crate::device::{FunctionDescription, LightMode, LightSnapshot, Power};

/// One currently usable member snapshot.
#[derive(Clone, Copy, Debug)]
pub(super) struct Member<'a> {
    pub(super) did: &'a str,
    pub(super) power: Power,
    pub(super) lighting: LightSnapshot,
    pub(super) supports_color: bool,
}

/// Derive group power, brightness, and one representative color snapshot.
pub(super) fn aggregate(
    description: &FunctionDescription,
    members: &[Member<'_>],
    confirmed_members: usize,
) -> (Power, LightSnapshot) {
    let mut state = LightSnapshot {
        brightness_percent: None,
        temperature_kelvin: None,
        color: None,
        mode: None,
    };
    if let Some(light) = representative(description, members) {
        state.temperature_kelvin = light.temperature_kelvin;
        state.color = light.color;
        state.mode = light.mode;
    }
    let power = if members.iter().any(|member| member.power == Power::On) {
        Power::On
    } else if members.len() == confirmed_members
        && members.iter().all(|member| member.power == Power::Off)
    {
        Power::Off
    } else {
        Power::Unknown
    };
    if description.supports_brightness() && power != Power::Unknown {
        state.brightness_percent = average_brightness(members, power);
    }
    (power, state)
}

/// Pick one color-capable member and keep all color fields from that member.
fn representative(
    description: &FunctionDescription,
    members: &[Member<'_>],
) -> Option<LightSnapshot> {
    members
        .iter()
        .filter(|member| member.supports_color)
        .map(|member| {
            let mut light = member.lighting;
            light.temperature_kelvin = light.temperature_kelvin.filter(|value| {
                description
                    .temperature_interval()
                    .is_some_and(|interval| interval.contains(*value))
            });
            if !description.supports_rgb() {
                light.color = None;
            }
            light.mode = light.mode.filter(|mode| match mode {
                LightMode::Temperature => description.supports_color_temperature(),
                LightMode::Rgb => description.supports_rgb(),
            });
            let valid = match light.mode {
                Some(LightMode::Temperature) => light.temperature_kelvin.is_some(),
                Some(LightMode::Rgb) => light.color.is_some(),
                None => false,
            };
            let priority = valid && member.power == Power::On;
            (!priority, member.did, light)
        })
        .min_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)))
        .map(|(_, _, light)| light)
}

/// Average available brightness for members matching the derived power.
fn average_brightness(members: &[Member<'_>], power: Power) -> Option<f64> {
    let (sum, count) = members
        .iter()
        .filter(|member| member.power == power)
        .filter_map(|member| member.lighting.brightness_percent)
        .fold((0.0, 0), |(sum, count), value| (sum + value, count + 1));
    (count > 0).then(|| sum / f64::from(count))
}

#[cfg(test)]
mod tests;
