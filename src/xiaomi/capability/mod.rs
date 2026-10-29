mod curtain;
mod description;
mod fan;
mod group;
mod light;
mod model;
mod native;
mod outlet;
mod patch;
mod repair;
mod sensor;
mod source;
mod state_sensor;
pub(crate) use group::parse_group;
pub(crate) use model::FanSpeedEncoding;
pub use model::{
    AppliedPatch, BatteryBinding, BrightnessBinding, CapabilityError, ColorBinding,
    CurtainBindings, CurtainMotorBinding, CurtainPositionBinding, CurtainStatusBinding,
    FanBindings, FanModeBinding, FanSpeedBinding, LightBindings, MeasurementBinding,
    MeasurementUnit, ModeBinding, NumericFormat, ParsedDevice, ParsedFunction, PropertyBinding,
    RecognitionDiagnostic, RecognitionMetadata, TemperatureBinding,
};
pub(crate) use native::NativeDescription;
pub(crate) use repair::{RepairPlan, compose_group_light};
pub(crate) use source::RawDescription;
pub use state_sensor::{
    EventArgument, EventBinding, MotionBinding, StateBinding, StateMapping, StateValue,
};

pub(crate) fn is_light_group(key: &crate::device::DeviceKey, model: &str) -> bool {
    key.id.starts_with("group.") || model == "mijia.light.group3"
}

pub fn parse_device_for_model(
    key: crate::device::DeviceKey,
    model: &str,
    instance: &serde_json::Value,
) -> Result<ParsedDevice, CapabilityError> {
    validate_source_identity(&key)?;
    // Groups are never classified here: admission comes only from confirmed
    // catalog relations, which call `parse_group` directly.
    let description = description::Description::parse(instance)?;
    let mut diagnostics = Vec::new();
    let mut patches = Vec::new();
    let (mut functions, refresh_trigger) =
        outlet::candidates(model, &description, &mut diagnostics, &mut patches);
    for service in &description.services {
        let diagnostic = |kind: &str, error: CapabilityError| RecognitionDiagnostic {
            target: format!("{kind}:{}", service.iid),
            reason: error.to_string(),
        };
        let candidate = if service.urn.standard("service", "light", 0x7802) {
            Some(
                light::parse_light_service(service, &mut diagnostics)
                    .map_err(|error| diagnostic("light", error)),
            )
        } else if service.urn.standard("service", "fan", 0x7808) {
            Some(
                fan::parse_fan_service(
                    model,
                    &description,
                    service,
                    &mut diagnostics,
                    &mut patches,
                )
                .map(|fan| {
                    ParsedFunction::fan_control(format!("fan:{}", service.iid), "风扇", fan)
                }),
            )
        } else if service.urn.standard("service", "curtain", 0x7816) {
            Some(
                curtain::parse_curtain_service(service)
                    .map_err(|error| diagnostic("curtain", error)),
            )
        } else {
            None
        };
        if let Some(candidate) = candidate {
            match candidate {
                Ok(function) => functions.push(function),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
    }
    patch::ls2307::apply(
        model,
        &description,
        &mut functions,
        &mut diagnostics,
        &mut patches,
    );
    functions.extend(sensor::measurements(&description, &mut diagnostics));
    functions.extend(state_sensor::functions(&description, &mut diagnostics));
    let mut owners = std::collections::BTreeMap::<PropertyBinding, Vec<usize>>::new();
    for (index, function) in functions.iter().enumerate() {
        for binding in function.bindings() {
            owners.entry(binding).or_default().push(index);
        }
    }
    let mut conflicts = std::collections::HashSet::new();
    for (binding, indexes) in owners {
        if indexes.len() > 1 {
            for index in indexes {
                conflicts.insert(index);
                diagnostics.push(RecognitionDiagnostic {
                    target: functions[index].id().into(),
                    reason: format!(
                        "binding {}.{} has ambiguous ownership",
                        binding.siid, binding.piid
                    ),
                });
            }
        }
    }
    functions = functions
        .into_iter()
        .enumerate()
        .filter_map(|(i, function)| (!conflicts.contains(&i)).then_some(function))
        .collect();
    patches.retain(|patch| {
        functions
            .iter()
            .any(|function| function.id() == patch.target)
    });
    if functions.is_empty() {
        return Err(if !diagnostics.is_empty() {
            CapabilityError::NoCapabilities(
                diagnostics
                    .into_iter()
                    .map(|issue| format!("{}: {}", issue.target, issue.reason))
                    .collect(),
            )
        } else {
            CapabilityError::UnsupportedDevice
        });
    }
    let (battery, mut battery_issue) = sensor::battery(&description);
    if battery_issue.as_deref() == Some("battery service is missing")
        && functions
            .iter()
            .all(|function| !function.kind().is_read_only())
    {
        battery_issue = None;
    }
    let core = functions
        .iter()
        .filter(|function| is_core(&description.urn, function))
        .map(|function| function.id().to_owned())
        .collect();
    let metadata = RecognitionMetadata {
        category: description.urn.name.into(),
        core,
        patches,
        diagnostics,
    };
    ParsedDevice::new_with_battery_diagnostic(
        key,
        functions,
        refresh_trigger,
        battery,
        battery_issue,
    )?
    .with_recognition(metadata)
}

fn validate_source_identity(key: &crate::device::DeviceKey) -> Result<(), CapabilityError> {
    if key.source != "xiaomi" || key.id.is_empty() || key.id.chars().any(char::is_control) {
        Err(CapabilityError::InvalidDescription("source identity"))
    } else {
        Ok(())
    }
}

fn is_core(urn: &description::Urn<'_>, function: &ParsedFunction) -> bool {
    use crate::device::FunctionKind;
    match function.kind() {
        FunctionKind::Light(_) => {
            urn.standard("device", "light", 0xa001) || urn.standard("device", "night-light", 0xa0ab)
        }
        FunctionKind::Fan { .. } => urn.standard("device", "fan", 0xa005),
        FunctionKind::Curtain => urn.standard("device", "curtain", 0xa00c),
        FunctionKind::Outlet => urn.standard("device", "outlet", 0xa002),
        FunctionKind::Temperature(_) | FunctionKind::Humidity(_) => {
            urn.standard("device", "temperature-humidity-sensor", 0xa00a)
        }
        FunctionKind::Contact => urn.standard("device", "magnet-sensor", 0xa016),
        FunctionKind::Motion => urn.standard("device", "motion-sensor", 0xa014),
        FunctionKind::Occupancy => urn.standard("device", "occupancy-sensor", 0xa0bf),
        FunctionKind::CarbonDioxide(_) | FunctionKind::Illuminance(_) => false,
    }
}

#[cfg(test)]
pub(crate) mod tests;
