use super::super::{
    diagnostics::FunctionDiagnostics,
    primitives::{known, on_off},
};
use crate::device::{FunctionDescription, HeaterCommand, HeaterSnapshot, TemperatureSourceState};

pub(in crate::terminal::bridge::render) fn heater_lines(
    description: &FunctionDescription,
    heater: HeaterSnapshot,
    diagnostics: &FunctionDiagnostics,
    pending: Vec<HeaterCommand>,
) -> Vec<String> {
    let mode = match heater.power.value {
        None => "unknown",
        Some(false) => "Off",
        Some(true) => "Heat",
    };
    let (current, current_source_time, current_received_at) = match heater.temperature_source {
        TemperatureSourceState::Available(reading) => (
            known(reading.value, |value| format!("{value:.2} °C")),
            reading.source_time,
            reading.received_at,
        ),
        TemperatureSourceState::AwaitingReading => (
            "unknown (awaiting a valid temperature reading)".into(),
            None,
            None,
        ),
        TemperatureSourceState::Unavailable => {
            ("unknown (built-in source unavailable)".into(), None, None)
        }
    };
    let mut entries = vec![
        (
            "Power",
            on_off(heater.power.value),
            heater.power.source_time,
            heater.power.received_at,
        ),
        (
            "Mode",
            mode.into(),
            heater.power.source_time,
            heater.power.received_at,
        ),
        (
            "Target temperature",
            known(heater.temperature.value, |value| format!("{value} °C")),
            heater.temperature.source_time,
            heater.temperature.received_at,
        ),
        (
            "Current temperature",
            current,
            current_source_time,
            current_received_at,
        ),
    ];
    if heater.temperature_source != TemperatureSourceState::Available(heater.current_temperature)
        && let Some(value) = heater.current_temperature.value
    {
        entries.push((
            "Last reading (diagnostic only)",
            format!("{value:.2} °C"),
            heater.current_temperature.source_time,
            heater.current_temperature.received_at,
        ));
    }
    let mut lines = diagnostics.observed_lines(description, entries);
    if !pending.is_empty() {
        lines.push(format!("    queued heater commands: {pending:?}."));
    }
    lines
}
