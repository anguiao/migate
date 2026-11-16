use super::{
    measurement_text,
    primitives::{known, observed_text},
};
use crate::device::{
    AmbientTemperature, BatterySnapshot, FunctionDescription, FunctionKey, FunctionKind,
    MeasurementSnapshot, StateInput, StateSnapshot,
};

pub(super) struct AmbientSource {
    pub(super) key: FunctionKey,
    pub(super) label: Option<String>,
    pub(super) last_reading: Option<MeasurementSnapshot>,
    pub(super) synchronization: String,
}

pub(super) fn ambient_lines(
    ambient: AmbientTemperature,
    source: Option<&AmbientSource>,
) -> Vec<String> {
    let Some(source) = source else {
        return vec!["    Ambient temperature: unknown (not bound); upstream time unknown; received at unknown.".into()];
    };
    let mut lines = vec![format!(
        "    Ambient source: {}/{}{}.",
        source.key.device.id,
        source.key.id,
        source
            .label
            .as_ref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default()
    )];
    let text = match ambient {
        AmbientTemperature::Available(reading) => measurement_text(reading, "°C"),
        AmbientTemperature::SourceMissing => "unknown (source not in current directory or access revoked); upstream time unknown; received at unknown".into(),
        AmbientTemperature::SourceUnavailable => "unknown (temperature source unavailable); upstream time unknown; received at unknown".into(),
        AmbientTemperature::AwaitingReading => "unknown (awaiting a valid temperature reading); upstream time unknown; received at unknown".into(),
        AmbientTemperature::Unbound => unreachable!("bound source has an ambient projection"),
    };
    lines.push(format!("    Ambient temperature: {text}."));
    if !matches!(ambient, AmbientTemperature::Available(_))
        && let Some(reading) = source.last_reading
        && reading.value.is_some()
    {
        lines.push(format!(
            "    Last reading (diagnostic only): {}.",
            measurement_text(reading, "°C")
        ));
    }
    lines.push(format!(
        "    Ambient source synchronization: {}.",
        source.synchronization
    ));
    lines
}

pub(super) fn battery_line(battery: BatterySnapshot) -> String {
    format!(
        "  battery (shared): {}",
        measurement_text(
            MeasurementSnapshot {
                value: battery.value,
                source_time: battery.source_time,
                received_at: battery.received_at,
            },
            "%"
        )
    )
}

pub(super) fn measurement_line(
    description: &FunctionDescription,
    measurement: MeasurementSnapshot,
    unit: &str,
) -> String {
    format!(
        "  {} ({}): {}",
        description.id,
        description.label,
        measurement_text(measurement, unit)
    )
}

pub(super) fn text(measurement: MeasurementSnapshot, unit: &str) -> String {
    let value = known(measurement.value, |value| format!("{value:.2} {unit}"));
    observed_text(&value, measurement.source_time, measurement.received_at)
}

pub(super) fn state_line(description: &FunctionDescription, state: StateSnapshot) -> String {
    let value = match (description.kind, state.active) {
        (_, None) => "unknown",
        (FunctionKind::Contact, Some(true)) => "open",
        (FunctionKind::Contact, Some(false)) => "closed",
        (FunctionKind::Motion, Some(true)) => "motion detected",
        (FunctionKind::Motion, Some(false)) => "no motion",
        (FunctionKind::Occupancy, Some(true)) => "occupied",
        (FunctionKind::Occupancy, Some(false)) => "unoccupied",
        _ => unreachable!("state text is only used for state functions"),
    };
    let input = match state.input {
        None => "native input unknown".into(),
        Some(StateInput::Property(value)) => format!("native property {value}"),
        Some(StateInput::Event { siid, eiid }) => format!("native event {siid}.{eiid}"),
    };
    let text = observed_text(
        &format!("{value}; {input}"),
        state.source_time,
        state.received_at,
    );
    format!("  {} ({}): {text}", description.id, description.label)
}
