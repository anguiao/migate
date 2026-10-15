use super::{
    state::{Context, DeviceRecord},
    work::{ConfirmedLight, DeviceWork},
};
use crate::{
    device::{
        Availability, CommandError, DeviceKey, LightColorTarget, LightDimension, LightMode,
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

/// One validated light report and whether it is an external operation.
pub(super) struct ObservedLight {
    pub(super) patch: LightPatch,
    pub(super) external: bool,
}

/// Validate at the MIoT boundary before counters or action ownership change.
pub(super) fn observe(
    work: &DeviceWork,
    id: &str,
    binding: PropertyBinding,
    value: &Value,
    origin: Origin,
) -> Result<ObservedLight, CapabilityError> {
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
        && work
            .light_write
            .as_ref()
            .is_some_and(|gate| gate.is_feedback(binding, value));
    Ok(ObservedLight {
        patch,
        external: matches!(origin, Origin::Notification) && changed && !feedback,
    })
}

/// Apply one validated report. `external` was decided without applying state;
/// values the logical description cannot express become unknown instead of
/// being rounded to a requested target.
pub(super) fn apply_report(
    work: &DeviceWork,
    id: &str,
    binding: PropertyBinding,
    mut patch: LightPatch,
    external: bool,
    origin: Origin,
) {
    let record = &work.record;
    let function = record.physical.function(id).unwrap();
    if let Some(value) = patch.temperature_kelvin
        && function
            .description()
            .temperature_range()
            .is_none_or(|range| !range.contains(value))
    {
        function.clear_light_temperature();
        patch.temperature_kelvin = None;
    }
    let before = function.snapshot();
    let state = before.lighting.unwrap();
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
    work.observations
        .observed(record, id, binding, matches!(origin, Origin::Notification));
    function
        .observe_light(patch)
        .expect("validated light report");
    if patch.mode.is_some() {
        refresh_missing_active(work, id, state.mode);
    }
}

/// Whether an observed patch differs from the prior confirmed fields; this is
/// the attribution basis for group-owned member operations, never an
/// optimistic accepted member target.
pub(super) fn confirmed_differs(confirmed: &ConfirmedLight, patch: LightPatch) -> bool {
    patch
        .power
        .is_some_and(|power| confirmed.power_value() != Some(power))
        || patch
            .brightness_percent
            .is_some_and(|value| confirmed.brightness_percent != Some(value))
        || patch
            .temperature_kelvin
            .is_some_and(|value| confirmed.temperature_kelvin != Some(value))
        || patch
            .color
            .is_some_and(|value| confirmed.color != Some(value))
        || patch
            .mode
            .is_some_and(|value| confirmed.mode != Some(value))
}

impl Context {
    /// New source targets take over other effective groups sharing members.
    /// Internal auxiliary batches never enter this command boundary.
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
        self.take_over_group_tracks(
            &members,
            Some(did),
            target.power == Some(false),
            target.brightness_percent.is_some(),
            target.color.is_some(),
        );
    }

    fn take_over_group_tracks(
        &self,
        members: &[String],
        except: Option<&str>,
        off: bool,
        brightness: bool,
        color: bool,
    ) {
        let mut dimensions = Vec::new();
        if brightness {
            dimensions.push(LightDimension::Brightness);
        }
        if color {
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
                    if except.is_some_and(|did| group_did == did)
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
                        description.has_capability(crate::device::Capability::Brightness)
                    }
                    LightDimension::Color => {
                        description.temperature_range().is_some()
                            || description.has_capability(crate::device::Capability::ColorRgb)
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

    /// A member operation observed on the wire stops the conflicting dimension
    /// of every effective group that shares the member. Stop stays local to the
    /// group the user targeted.
    pub(super) fn take_over_member_groups(&self, member_did: &str, patch: LightPatch) {
        self.take_over_group_tracks(
            &[member_did.to_owned()],
            None,
            patch.power == Some(false),
            patch.brightness_percent.is_some(),
            patch.mode.is_some() || patch.color.is_some() || patch.temperature_kelvin.is_some(),
        );
    }

    /// Recompute repaired state for every group sharing this member after a
    /// member observation or availability change.
    pub(super) fn refresh_repair_groups(&self, member_did: &str) {
        let groups = {
            let live = self.live.borrow();
            live.devices
                .iter()
                .filter(|(_, slot)| {
                    slot.effective_repair.is_some()
                        && slot.effective_group.as_ref().is_some_and(|group| {
                            group.members.iter().any(|member| member == member_did)
                        })
                })
                .map(|(did, _)| did.clone())
                .collect::<Vec<_>>()
        };
        for did in groups {
            self.refresh_repair_group(&did);
        }
    }

    /// Apply the confirmed representative's color and mode to one repaired
    /// group. Losing or replacing the representative clears the old values
    /// first; the native power, brightness and color temperature stay.
    pub(super) fn refresh_repair_group(&self, group_did: &str) {
        let (plan, function) = {
            let live = self.live.borrow();
            let Some(slot) = live.devices.get(group_did) else {
                return;
            };
            let Some(plan) = slot.effective_repair.clone() else {
                return;
            };
            let Some(function) = slot
                .admitted
                .as_ref()
                .and_then(|record| record.physical.function("main"))
            else {
                return;
            };
            (plan, function)
        };
        let representative = self.representative(&plan);
        let changed = self
            .live
            .borrow()
            .devices
            .get(group_did)
            .is_none_or(|slot| slot.representative != representative);
        if changed {
            function.clear_light_color();
            if let Some(slot) = self.live.borrow_mut().devices.get_mut(group_did) {
                slot.representative = representative.clone();
            }
        }
        let Some(confirmed) = representative
            .as_ref()
            .and_then(|key| self.confirmed_member(key))
        else {
            return;
        };
        let previous = function.snapshot().lighting.unwrap();
        if (previous.mode.is_some() && confirmed.mode.is_none())
            || (previous.color.is_some() && confirmed.color.is_none())
        {
            // Missing fields in the current confirmed authority cannot retain
            // another authority's mode or inactive RGB cache.
            function.clear_light_color();
        }
        let patch = LightPatch {
            // The chosen member's cached RGB travels even while its current
            // mode is white; fields are never combined across members.
            color: confirmed.color,
            mode: confirmed.mode,
            ..LightPatch::default()
        };
        if let Err(error) = function.observe_light(patch) {
            log::warn!("repaired group {group_did} state rejected: {error}");
        }
    }

    fn confirmed_member(&self, key: &DeviceKey) -> Option<ConfirmedLight> {
        let live = self.live.borrow();
        let slot = live.devices.get(&key.id)?;
        let record = slot.admitted.as_ref()?;
        if record.life.is_closed() || record.physical.availability() != Availability::Available {
            return None;
        }
        slot.work.as_ref()?.confirmed_light()
    }

    /// Prefer an ON member whose current mode and matching value are both
    /// confirmed, then the other readable members by stable DID order.
    fn representative(&self, plan: &crate::xiaomi::capability::RepairPlan) -> Option<DeviceKey> {
        let live = self.live.borrow();
        let mut candidates = Vec::new();
        for key in &plan.members {
            let Some(slot) = live.devices.get(&key.id) else {
                continue;
            };
            let Some(record) = &slot.admitted else {
                continue;
            };
            if record.life.is_closed() || record.physical.availability() != Availability::Available
            {
                continue;
            }
            let Some(confirmed) = slot.work.as_ref().and_then(|work| work.confirmed_light()) else {
                continue;
            };
            let valid = match confirmed.mode {
                Some(LightMode::Temperature) => confirmed.temperature_kelvin.is_some(),
                Some(LightMode::Rgb) => confirmed.color.is_some(),
                None => false,
            };
            // Exact two tiers: ON with a valid current mode and matching
            // value first, then every other readable confirmed member
            // (unknown mode stays unknown). Stable DeviceKey within each tier.
            let priority = valid && confirmed.power_value() == Some(true);
            candidates.push((priority, key.id.clone(), key.clone()));
        }
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        candidates.into_iter().next().map(|(_, _, key)| key)
    }
}
