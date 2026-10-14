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
    let mut values = function.power.into_iter().collect::<Vec<_>>();
    if let Some(light) = &function.lighting {
        values.extend(
            light
                .brightness
                .as_ref()
                .map(|brightness| brightness.property),
        );
        values.extend(
            light
                .temperature
                .as_ref()
                .map(|temperature| temperature.property),
        );
        values.extend(light.color.as_ref().map(|color| color.property));
        values.extend(light.mode.as_ref().map(|mode| mode.property));
    }
    values
}
pub(super) fn decode(
    function: &ParsedFunction,
    binding: PropertyBinding,
    value: &Value,
) -> Result<LightPatch, CapabilityError> {
    let mut patch = LightPatch::default();
    if function.power == Some(binding) {
        patch.power = Some(
            value
                .as_bool()
                .ok_or(CapabilityError::InvalidValue("power type"))?,
        );
        return Ok(patch);
    }
    let light = function
        .lighting
        .as_ref()
        .ok_or(CapabilityError::InvalidValue("binding"))?;
    if let Some(brightness) = light
        .brightness
        .as_ref()
        .filter(|brightness| brightness.property == binding)
    {
        patch.brightness_percent = Some(brightness.brightness_from_raw(value)?);
    } else if let Some(temperature) = light
        .temperature
        .as_ref()
        .filter(|temperature| temperature.property == binding)
    {
        patch.temperature_kelvin = Some(temperature.read_raw(value)?);
    } else if let Some(color) = light
        .color
        .as_ref()
        .filter(|color| color.property == binding)
    {
        patch.color = Rgb::from_packed(color.read_raw(value)?);
    } else if let Some(mode) = light.mode.as_ref().filter(|mode| mode.property == binding) {
        patch.mode = Some(mode.read_mode(value)?);
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
        .ok_or(CommandError::Unsupported)?;
    let light = function.lighting.as_ref();
    let snapshot = record
        .physical
        .function(id)
        .ok_or(CommandError::Unsupported)?
        .fresh_snapshot()
        .snapshot;
    let state = snapshot.lighting;
    let brightness = light.and_then(|light| light.brightness.as_ref());
    let temperature = light.and_then(|light| light.temperature.as_ref());
    let color = light.and_then(|light| light.color.as_ref());
    let mode_binding = light.and_then(|light| light.mode.as_ref());
    let convert = |error: CapabilityError| CommandError::Rejected(error.to_string());
    let mut values = Vec::new();
    if let Some(power) = target.power {
        let binding = function.power.ok_or(CommandError::Unsupported)?;
        values.push((binding, json!(power)));
    }
    if let Some(percent) = target.brightness_percent {
        let binding = brightness.ok_or(CommandError::Unsupported)?;
        values.push((
            binding.property,
            json!(binding.brightness_to_raw(percent).map_err(convert)?),
        ));
    }
    let mode = match target.color {
        Some(LightColorTarget::Temperature(kelvin)) => {
            let binding = temperature.ok_or(CommandError::Unsupported)?;
            values.push((
                binding.property,
                json!(binding.temperature_to_raw(kelvin).map_err(convert)?),
            ));
            Some(LightMode::Temperature)
        }
        Some(LightColorTarget::Rgb(rgb)) => {
            let binding = color.ok_or(CommandError::Unsupported)?;
            values.push((binding.property, json!(rgb.packed())));
            Some(LightMode::Rgb)
        }
        None => None,
    };
    if let Some(mode) = mode
        && state.and_then(|state| state.mode) != Some(mode)
        && let Some(binding) = mode_binding
    {
        values.push((
            binding.property,
            json!(match mode {
                LightMode::Temperature => binding.temperature_value,
                LightMode::Rgb => binding.rgb_value,
            }),
        ));
    }
    values.retain(|(property, value)| {
        let current = if function.power == Some(*property) {
            snapshot.power.value().map(Value::Bool)
        } else if brightness.is_some_and(|binding| binding.property == *property) {
            state
                .and_then(|state| state.brightness_percent)
                .and_then(|percent| brightness.unwrap().brightness_to_raw(percent).ok())
                .map(|raw| json!(raw))
        } else if temperature.is_some_and(|binding| binding.property == *property) {
            state
                .and_then(|state| state.temperature_kelvin)
                .map(|value| json!(value))
        } else if color.is_some_and(|binding| binding.property == *property) {
            state
                .and_then(|state| state.color)
                .map(|value| json!(value.packed()))
        } else if mode_binding.is_some_and(|binding| binding.property == *property) {
            state.and_then(|state| state.mode).map(|mode| {
                json!(match mode {
                    LightMode::Temperature => mode_binding.unwrap().temperature_value,
                    LightMode::Rgb => mode_binding.unwrap().rgb_value,
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

/// A mode change needs a fresh active value; repeated demand shares an existing read.
pub(super) fn refresh_missing_active(
    work: &DeviceWork,
    id: &str,
    previous_mode: Option<LightMode>,
) {
    let Some(state) = work
        .record
        .physical
        .function(id)
        .and_then(|function| function.snapshot().lighting)
    else {
        return;
    };
    let cached = work.record.cached.borrow();
    let Some(light) = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .and_then(|function| function.lighting.as_ref())
    else {
        return;
    };
    let binding = match state.mode {
        Some(LightMode::Temperature) if state.temperature_kelvin.is_none() => light
            .temperature
            .as_ref()
            .map(|temperature| temperature.property),
        Some(LightMode::Rgb) if state.color.is_none() => {
            light.color.as_ref().map(|color| color.property)
        }
        _ => None,
    };
    drop(cached);
    if let Some(binding) = binding {
        if previous_mode != state.mode {
            work.refresh_property(id, binding);
        } else {
            work.ensure_property_read(id, binding);
        }
    }
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
    drop(cached);
    let function = record.physical.function(id).unwrap();
    let before = function.snapshot();
    let state = before.lighting;
    let changed = patch
        .power
        .is_some_and(|power| before.power.value() != Some(power))
        || patch
            .brightness_percent
            .is_some_and(|value| state.and_then(|state| state.brightness_percent) != Some(value))
        || patch
            .temperature_kelvin
            .is_some_and(|value| state.and_then(|state| state.temperature_kelvin) != Some(value))
        || patch
            .color
            .is_some_and(|value| state.and_then(|state| state.color) != Some(value))
        || patch
            .mode
            .is_some_and(|value| state.and_then(|state| state.mode) != Some(value));
    let feedback = matches!(origin, Origin::Notification)
        && work
            .light_write
            .as_ref()
            .is_some_and(|gate| gate.is_feedback(binding, value));
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
            || (patch.temperature_kelvin.is_some()
                && state.and_then(|state| state.mode) == Some(LightMode::Temperature))
            || (patch.color.is_some()
                && state.and_then(|state| state.mode) == Some(LightMode::Rgb));
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
        refresh_missing_active(work, id, state.and_then(|state| state.mode));
    }
    Ok(())
}
