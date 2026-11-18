use super::super::{
    diagnostics::FunctionDiagnostics,
    measurement::{AmbientSource, ambient_lines},
    primitives::on_off,
};
use crate::device::{
    AirConditionerCommand, AirConditionerFanSpeed, AirConditionerMode, AirConditionerSnapshot,
    AmbientTemperature, FunctionDescription, FunctionKind,
    air_conditioner::AirConditionerConstraints,
};
use crate::{terminal::status::property_diagnostics, xiaomi::adapter::PropertyRole};

pub(in crate::terminal::bridge::render) struct AirConditionerControl {
    pub(in crate::terminal::bridge::render) constraints_ready: bool,
    pub(in crate::terminal::bridge::render) constraints: Option<AirConditionerConstraints>,
    pub(in crate::terminal::bridge::render) pending: Vec<AirConditionerCommand>,
}

pub(in crate::terminal::bridge::render) fn air_conditioner_lines(
    description: &FunctionDescription,
    ac: AirConditionerSnapshot,
    ambient: AmbientTemperature,
    source: Option<&AmbientSource>,
    diagnostics: &FunctionDiagnostics,
    control: AirConditionerControl,
) -> Vec<String> {
    let mut entries = vec![
        diagnostics.observation(PropertyRole::Power, ac.power, on_off),
        diagnostics.observation(PropertyRole::Mode, ac.mode, |value| format!("{value:?}")),
        diagnostics.observation(PropertyRole::TargetTemperature, ac.temperature, |value| {
            format!("{value} °C")
        }),
        diagnostics.observation(PropertyRole::Speed, ac.speed, |value| format!("{value:?}")),
    ];
    if matches!(
        description.kind,
        FunctionKind::AirConditioner {
            vertical_swing: true,
            ..
        }
    ) {
        entries.push(diagnostics.observation(PropertyRole::VerticalSwing, ac.swing, on_off));
    }
    let mut lines = diagnostics.observed_lines(description, entries);
    lines.extend(ambient_lines(ambient, source));
    if ac.power.value == Some(true) && ac.speed.value == Some(AirConditionerFanSpeed::Auto) {
        lines.push("    Current fan level: unknown (automatic speed).".into());
    }
    if let Some(range) = diagnostics.property(PropertyRole::ControlRange) {
        lines.push(format!(
            "    Control range source: {}.",
            property_diagnostics(Some(range))
        ));
    }
    if !control.constraints_ready {
        lines.push("    Control range is awaiting source confirmation.".into());
    }
    if let Some(constraints) = control.constraints {
        for mode in AirConditionerMode::ALL {
            if let Some(temperatures) = constraints.temperatures(mode) {
                lines.push(format!(
                    "    {mode:?}: temperatures {temperatures:?} °C; speeds {:?}.",
                    constraints.speeds(mode).unwrap()
                ));
            }
        }
    } else {
        lines.push("    Control range: unknown.".into());
    }
    if !control.pending.is_empty() {
        lines.push(format!(
            "    queued air-conditioner commands: {:?}.",
            control.pending
        ));
    }
    lines
}
