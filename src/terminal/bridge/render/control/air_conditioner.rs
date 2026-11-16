use super::super::{
    diagnostics::FunctionDiagnostics,
    measurement::{AmbientSource, ambient_lines},
    primitives::{known, on_off},
};
use crate::device::{
    AirConditionerCommand, AirConditionerFanSpeed, AirConditionerMode, AirConditionerSnapshot,
    AmbientTemperature, FunctionDescription, FunctionKind,
    air_conditioner::AirConditionerConstraints,
};

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
        (
            "Power",
            on_off(ac.power.value),
            ac.power.source_time,
            ac.power.received_at,
        ),
        (
            "Mode",
            known(ac.mode.value, |value| format!("{value:?}")),
            ac.mode.source_time,
            ac.mode.received_at,
        ),
        (
            "Target temperature",
            known(ac.temperature.value, |value| format!("{value} °C")),
            ac.temperature.source_time,
            ac.temperature.received_at,
        ),
        (
            "Speed",
            known(ac.speed.value, |value| format!("{value:?}")),
            ac.speed.source_time,
            ac.speed.received_at,
        ),
    ];
    if matches!(
        description.kind,
        FunctionKind::AirConditioner {
            vertical_swing: true,
            ..
        }
    ) {
        entries.push((
            "Vertical swing",
            on_off(ac.swing.value),
            ac.swing.source_time,
            ac.swing.received_at,
        ));
    }
    let mut lines = diagnostics.observed_lines(description, entries);
    lines.extend(ambient_lines(ambient, source));
    if ac.power.value == Some(true) && ac.speed.value == Some(AirConditionerFanSpeed::Auto) {
        lines.push("    Current fan level: unknown (automatic speed).".into());
    }
    if diagnostics.property("Control range").is_some() {
        lines.push(format!(
            "    Control range source: {}.",
            diagnostics.property_text("Control range")
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
