use super::{state::Context, work::DeviceWork};
use crate::{
    device::{
        Availability, Capability, CommandError, LightColorTarget, LightDimension, LightMode,
        LightPatch, LightTarget, Rgb,
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
        values.extend(light.brightness.as_ref().map(|binding| binding.property));
        values.extend(light.temperature.as_ref().map(|binding| binding.property));
        values.extend(light.color.as_ref().map(|binding| binding.property));
        values.extend(light.mode.as_ref().map(|binding| binding.property));
    }
    values
}
pub(super) fn decode(
    function: &ParsedFunction,
    binding: PropertyBinding,
    value: &Value,
) -> Result<LightPatch, CapabilityError> {
    let mut patch = LightPatch::default();
    if binding == function.power {
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
    if let Some(brightness) = &light.brightness
        && brightness.property == binding
    {
        patch.brightness_percent = Some(brightness.brightness_from_raw(value)?);
    } else if let Some(temperature) = &light.temperature
        && temperature.property == binding
    {
        patch.temperature_kelvin = Some(temperature.read_raw(value)?);
    } else if let Some(color) = &light.color
        && color.property == binding
    {
        patch.color = Rgb::from_packed(color.read_raw(value)?);
    } else if let Some(mode) = &light.mode
        && mode.property == binding
    {
        patch.mode = Some(mode.read_mode(value)?);
    } else {
        return Err(CapabilityError::InvalidValue("binding"));
    }
    Ok(patch)
}
pub(super) fn encode(
    work: &DeviceWork,
    id: &str,
    target: LightTarget,
    force: bool,
) -> Result<Vec<(PropertyBinding, Value)>, CommandError> {
    let record = &work.record;
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
        let brightness = light.brightness.as_ref().ok_or(CommandError::Unsupported)?;
        values.push((
            brightness.property,
            json!(brightness.brightness_to_raw(percent).map_err(convert)?),
        ));
    }
    let mode = match target.color {
        Some(LightColorTarget::Temperature(kelvin)) => {
            let temperature = light
                .temperature
                .as_ref()
                .ok_or(CommandError::Unsupported)?;
            values.push((
                temperature.property,
                json!(temperature.temperature_to_raw(kelvin).map_err(convert)?),
            ));
            Some(LightMode::Temperature)
        }
        Some(LightColorTarget::Rgb(rgb)) => {
            let color = light.color.as_ref().ok_or(CommandError::Unsupported)?;
            values.push((color.property, json!(rgb.packed())));
            Some(LightMode::Rgb)
        }
        None => None,
    };
    if let Some(mode) = mode
        && let Some(binding) = light.mode
        && (force
            || work
                .observations
                .has_unconfirmed_write(id, binding.property)
            || state.mode != Some(mode))
    {
        values.push((
            binding.property,
            json!(match mode {
                LightMode::Temperature => binding.temperature_value,
                LightMode::Rgb => binding.rgb_value,
            }),
        ));
    }
    if force {
        return Ok(values);
    }
    values.retain(|(binding, value)| {
        if work.observations.has_unconfirmed_write(id, *binding) {
            return true;
        }
        let current = if *binding == function.power {
            snapshot.power.value().map(Value::Bool)
        } else if light
            .brightness
            .as_ref()
            .is_some_and(|brightness| brightness.property == *binding)
        {
            light
                .brightness
                .as_ref()
                .and_then(|brightness| {
                    state
                        .brightness_percent
                        .and_then(|percent| brightness.brightness_to_raw(percent).ok())
                })
                .map(|raw| json!(raw))
        } else if light
            .temperature
            .as_ref()
            .is_some_and(|temperature| temperature.property == *binding)
        {
            state.temperature_kelvin.map(|value| json!(value))
        } else if light
            .color
            .as_ref()
            .is_some_and(|color| color.property == *binding)
        {
            state.color.map(|value| json!(value.packed()))
        } else if light
            .mode
            .as_ref()
            .is_some_and(|mode| mode.property == *binding)
        {
            let binding = light.mode.expect("mode binding exists");
            state.mode.map(|mode| {
                json!(match mode {
                    LightMode::Temperature => binding.temperature_value,
                    LightMode::Rgb => binding.rgb_value,
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
    let state = work
        .record
        .physical
        .function(id)
        .unwrap()
        .snapshot()
        .lighting
        .unwrap();
    let cached = work.record.cached.borrow();
    let light = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .unwrap()
        .lighting
        .as_ref()
        .unwrap();
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
    takeover: bool,
) -> Result<(), CapabilityError> {
    let record = &work.record;
    let cached = record.cached.borrow();
    let description = cached
        .device
        .functions
        .iter()
        .find(|function| function.description.id == id)
        .unwrap();
    let mut patch = decode(description, binding, value)?;
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
    let feedback = takeover
        && matches!(origin, Origin::Notification)
        && work
            .light_write
            .as_ref()
            .is_some_and(|gate| gate.is_feedback(binding, value));
    let external = takeover && matches!(origin, Origin::Notification) && changed && !feedback;
    if let Some(value) = patch.temperature_kelvin
        && function
            .description()
            .temperature_range()
            .is_none_or(|range| !range.contains(value))
    {
        function.clear_light_temperature();
        patch.temperature_kelvin = None;
    }
    if external {
        if patch.power == Some(false) {
            function.take_over_light_all().expect("light");
        }
        if patch.brightness_percent.is_some() {
            function
                .take_over_light(LightDimension::Brightness)
                .expect("light");
        }
        let active_color = patch.mode.is_some()
            || (patch.temperature_kelvin.is_some() && state.mode == Some(LightMode::Temperature))
            || (patch.color.is_some() && state.mode == Some(LightMode::Rgb));
        if active_color {
            function
                .take_over_light(LightDimension::Color)
                .expect("light");
        }
    }
    work.observations.observed(record, id, binding, origin);
    function
        .observe_light(patch)
        .expect("validated light report");
    if patch.mode.is_some() {
        refresh_missing_active(work, id, state.mode);
    }
    Ok(())
}

impl Context {
    pub(super) fn is_independent_light(&self, did: &str) -> bool {
        let live = self.live.borrow();
        live.devices
            .get(did)
            .is_some_and(|slot| slot.effective_group.is_none())
            && !live
                .devices
                .values()
                .filter_map(|slot| slot.effective_group.as_ref())
                .any(|group| group.members.iter().any(|member| member == did))
    }

    /// New source targets take over other effective groups sharing members.
    pub(super) fn take_over_related_targets(&self, did: &str, target: LightTarget) {
        let members = {
            let live = self.live.borrow();
            match live
                .devices
                .get(did)
                .and_then(|slot| slot.effective_group.as_ref())
            {
                Some(group) => group.members.clone(),
                None => vec![did.to_owned()],
            }
        };
        let off = target.power == Some(false);
        let mut dimensions = Vec::new();
        if target.brightness_percent.is_some() {
            dimensions.push(LightDimension::Brightness);
        }
        if target.color.is_some() {
            dimensions.push(LightDimension::Color);
        }
        if !off && dimensions.is_empty() {
            return;
        }
        let groups = {
            let live = self.live.borrow();
            live.devices
                .iter()
                .filter_map(|(group_did, slot)| {
                    let group = slot.effective_group.as_ref()?;
                    if group_did == did
                        || !group.members.iter().any(|member| members.contains(member))
                    {
                        return None;
                    }
                    Some((
                        slot.admitted.as_ref()?.physical.function("main")?,
                        slot.work.clone(),
                    ))
                })
                .collect::<Vec<_>>()
        };
        for (function, work) in groups {
            if off {
                if let Some(work) = &work {
                    work.light_frames.borrow_mut().cancel_all();
                }
                function.take_over_light_all().expect("native light group");
                continue;
            }
            for dimension in &dimensions {
                let description = function.description();
                let supported = match dimension {
                    LightDimension::Brightness => {
                        description.has_capability(Capability::Brightness)
                    }
                    LightDimension::Color => {
                        description.temperature_range().is_some()
                            || description.has_capability(Capability::ColorRgb)
                    }
                };
                if supported {
                    if let Some(work) = &work {
                        work.light_frames.borrow_mut().cancel_dimension(*dimension);
                    }
                    function
                        .take_over_light(*dimension)
                        .expect("supported group dimension");
                }
            }
        }
    }

    /// Recompute a source group's repaired state and groups sharing this source
    /// after an observation or availability change.
    pub(super) fn refresh_repair_groups(&self, member_did: &str) {
        let groups = {
            let live = self.live.borrow();
            live.devices
                .iter()
                .filter(|(did, slot)| {
                    slot.effective_repair.is_some()
                        && (*did == member_did
                            || slot.effective_group.as_ref().is_some_and(|group| {
                                group.members.iter().any(|member| member == member_did)
                            }))
                })
                .map(|(did, _)| did.clone())
                .collect::<Vec<_>>()
        };
        for did in groups {
            self.refresh_repair_group(&did);
        }
    }

    /// Replace RGB and mode together from one current member snapshot.
    pub(super) fn refresh_repair_group(&self, group_did: &str) {
        let live = self.live.borrow();
        let Some(slot) = live.devices.get(group_did) else {
            return;
        };
        let Some(plan) = &slot.effective_repair else {
            return;
        };
        let Some(record) = &slot.admitted else {
            return;
        };
        let function = record.physical.function("main").unwrap();
        if record.life.is_closed() || record.physical.availability() != Availability::Available {
            function.replace_light_color(None, None);
            return;
        }
        let representative = plan
            .members
            .iter()
            .filter_map(|key| {
                let slot = live.devices.get(&key.id)?;
                let record = slot.admitted.as_ref()?;
                if record.life.is_closed()
                    || record.physical.availability() != Availability::Available
                    || slot.work.as_ref().is_none_or(|work| work.life.is_closed())
                {
                    return None;
                }
                let snapshot = record.physical.function("main").unwrap().snapshot();
                let light = snapshot.lighting.unwrap();
                let valid = match light.mode {
                    Some(LightMode::Temperature) => light.temperature_kelvin.is_some(),
                    Some(LightMode::Rgb) => light.color.is_some(),
                    None => false,
                };
                let priority = valid && snapshot.power.value() == Some(true);
                Some((!priority, &key.id, light))
            })
            .min_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)))
            .map(|(_, _, light)| light);
        let (color, mode) = representative.map_or((None, None), |light| (light.color, light.mode));
        function.replace_light_color(color, mode);
    }
}
