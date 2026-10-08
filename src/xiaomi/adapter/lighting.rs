use super::{state::DeviceRecord, work::DeviceWork};
use crate::{
    device::{
        CommandError, LightColorTarget, LightDimension, LightMode, LightPatch, LightTarget, Rgb,
    },
    xiaomi::capability::{CapabilityError, ParsedFunction, PropertyBinding},
};
use serde_json::{Value, json};

pub(super) type PropertyKey = (String, u32, u32);
pub(super) fn key(id: &str, binding: PropertyBinding) -> PropertyKey {
    (id.into(), binding.siid, binding.piid)
}
pub(super) fn bindings(function: &ParsedFunction) -> Vec<PropertyBinding> {
    let mut values = vec![function.power];
    if let Some(light) = &function.lighting {
        values.extend([light.brightness.property, light.temperature.property]);
        values.extend(light.color.map(|value| value.property));
        values.extend(light.mode.map(|value| value.property));
    }
    values
}
pub(super) fn decode(
    function: &ParsedFunction,
    binding: PropertyBinding,
    value: &Value,
) -> Result<LightPatch, CapabilityError> {
    let light = function
        .lighting
        .as_ref()
        .ok_or(CapabilityError::InvalidValue("light"))?;
    let mut patch = LightPatch::default();
    if binding == function.power {
        patch.power = Some(
            value
                .as_bool()
                .ok_or(CapabilityError::InvalidValue("power type"))?,
        );
    } else if binding == light.brightness.property {
        patch.brightness_percent = Some(light.brightness.brightness_from_raw(value)?);
    } else if binding == light.temperature.property {
        patch.temperature_kelvin = Some(light.temperature.read_raw(value)?);
    } else if light.color.is_some_and(|color| color.property == binding) {
        patch.color = Rgb::from_packed(light.color.unwrap().read_raw(value)?);
    } else if light.mode.is_some_and(|mode| mode.property == binding) {
        patch.mode = Some(light.mode.unwrap().read_mode(value)?);
    } else {
        return Err(CapabilityError::InvalidValue("binding"));
    }
    Ok(patch)
}
pub(super) fn merge(patch: &mut LightPatch, next: LightPatch) {
    patch.power = next.power.or(patch.power);
    patch.brightness_percent = next.brightness_percent.or(patch.brightness_percent);
    patch.temperature_kelvin = next.temperature_kelvin.or(patch.temperature_kelvin);
    patch.color = next.color.or(patch.color);
    patch.mode = next.mode.or(patch.mode);
}
pub(super) fn encode(
    record: &DeviceRecord,
    id: &str,
    target: LightTarget,
) -> Result<Vec<(PropertyBinding, Value)>, CommandError> {
    let cached = record.cached.borrow();
    let function = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .unwrap();
    let light = function.lighting.as_ref().unwrap();
    let snapshot = record.physical.function(id).unwrap().snapshot();
    let state = snapshot.lighting.unwrap();
    let convert = |error: CapabilityError| CommandError::Rejected(error.to_string());
    let mut values = Vec::new();
    if let Some(power) = target.power {
        values.push((function.power, json!(power)));
    }
    if let Some(percent) = target.brightness_percent {
        values.push((
            light.brightness.property,
            json!(
                light
                    .brightness
                    .brightness_to_raw(percent)
                    .map_err(convert)?
            ),
        ));
    }
    let mode = match target.color {
        Some(LightColorTarget::Temperature(kelvin)) => {
            values.push((
                light.temperature.property,
                json!(
                    light
                        .temperature
                        .temperature_to_raw(kelvin)
                        .map_err(convert)?
                ),
            ));
            Some(LightMode::Temperature)
        }
        Some(LightColorTarget::Rgb(rgb)) => {
            let color = light.color.ok_or(CommandError::Unsupported)?;
            values.push((color.property, json!(rgb.packed())));
            Some(LightMode::Rgb)
        }
        None => None,
    };
    if let Some(mode) = mode
        && state.mode != Some(mode)
        && let Some(binding) = light.mode
    {
        values.push((
            binding.property,
            json!(match mode {
                LightMode::Temperature => binding.temperature_value,
                LightMode::Rgb => binding.rgb_value,
            }),
        ));
    }
    values.retain(|(binding, value)| {
        let current = if *binding == function.power {
            snapshot.power.value().map(Value::Bool)
        } else if *binding == light.brightness.property {
            state
                .brightness_percent
                .and_then(|percent| light.brightness.brightness_to_raw(percent).ok())
                .map(|raw| json!(raw))
        } else if *binding == light.temperature.property {
            state.temperature_kelvin.map(|value| json!(value))
        } else if light.color.is_some_and(|color| color.property == *binding) {
            state.color.map(|value| json!(value.packed()))
        } else if light.mode.is_some_and(|mode| mode.property == *binding) {
            state.mode.map(|mode| {
                json!(match mode {
                    LightMode::Temperature => light.mode.unwrap().temperature_value,
                    LightMode::Rgb => light.mode.unwrap().rgb_value,
                })
            })
        } else {
            None
        };
        current.as_ref() != Some(value)
    });
    Ok(values)
}
#[derive(Clone, Copy)]
pub(super) enum Origin {
    Read,
    Notification,
    Cache,
}

/// Validate at the MIoT boundary before counters or action ownership change.
pub(super) fn observe(
    work: &DeviceWork,
    id: &str,
    binding: PropertyBinding,
    value: &Value,
    origin: Origin,
) -> Result<(), CapabilityError> {
    let record = &work.record;
    let cached = record.cached.borrow();
    let description = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .unwrap();
    let patch = decode(description, binding, value)?;
    let bindings = description.lighting.as_ref().unwrap().clone();
    drop(cached);
    let function = record.physical.function(id).unwrap();
    let before = function.snapshot();
    let state = before.lighting.unwrap();
    let changed = patch
        .power
        .is_some_and(|power| before.power.value() != Some(power))
        || patch
            .brightness_percent
            .is_some_and(|value| state.brightness_percent != Some(value))
        || patch
            .temperature_kelvin
            .is_some_and(|value| state.temperature_kelvin != Some(value))
        || patch.color.is_some_and(|value| state.color != Some(value))
        || patch.mode.is_some_and(|value| state.mode != Some(value));
    let feedback = matches!(origin, Origin::Notification)
        && work.observations.is_feedback(record, id, binding, value);
    if matches!(origin, Origin::Notification) && changed && !feedback {
        if patch.power == Some(false) {
            function.take_over_light_all().expect("light");
        }
        if patch.brightness_percent.is_some() {
            function
                .take_over_light(LightDimension::Brightness)
                .expect("light");
        }
        let active_color = patch.mode.is_some()
            || (patch.temperature_kelvin.is_some() && state.mode != Some(LightMode::Rgb))
            || (patch.color.is_some() && state.mode != Some(LightMode::Temperature));
        if active_color {
            function
                .take_over_light(LightDimension::Color)
                .expect("light");
        }
    }
    work.observations
        .observed(record, id, binding, matches!(origin, Origin::Notification));
    function
        .observe_light(patch)
        .expect("validated light report");
    if patch.mode.is_some() {
        let state = function.snapshot().lighting.unwrap();
        if state.mode == Some(LightMode::Temperature) && state.temperature_kelvin.is_none() {
            work.refresh_property(id, bindings.temperature.property);
        } else if state.mode == Some(LightMode::Rgb) && state.color.is_none() {
            work.refresh_property(id, bindings.color.unwrap().property);
        }
    }
    Ok(())
}
