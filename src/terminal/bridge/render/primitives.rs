use crate::{
    device::{Capability, FunctionDescription, Power},
    terminal::status::timestamp,
};

pub(super) type Observation = (&'static str, String, Option<i64>, Option<i64>);

pub(super) fn known<T>(value: Option<T>, format: impl FnOnce(T) -> String) -> String {
    value.map_or_else(|| "unknown".into(), format)
}

pub(super) fn on_off(value: Option<bool>) -> String {
    known(value, |value| if value { "on" } else { "off" }.into())
}

pub(super) fn power_text(power: Power) -> &'static str {
    match power {
        Power::Unknown => "unknown",
        Power::On => "on",
        Power::Off => "off",
    }
}

pub(super) fn observed_text(
    value: &str,
    source_time: Option<i64>,
    received_at: Option<i64>,
) -> String {
    let source = known(source_time, |time| time.to_string());
    let received = known(received_at, timestamp);
    format!("{value}; upstream time {source}; received at {received}")
}

pub(super) fn capabilities(description: &FunctionDescription) -> String {
    description
        .capabilities()
        .iter()
        .map(|capability| match capability {
            Capability::OnOff => "on/off".into(),
            Capability::Curtain => "curtain".into(),
            Capability::Fan => "fan".into(),
            Capability::AirConditioner => "air conditioner".into(),
            Capability::Heater => "heater".into(),
            Capability::Brightness => "brightness".into(),
            Capability::ColorTemperature(interval) => {
                format!("temperature {}–{} K", interval.min(), interval.max())
            }
            Capability::ColorRgb => "RGB".into(),
            Capability::Temperature => "temperature".into(),
            Capability::Humidity => "humidity".into(),
            Capability::CarbonDioxide => "co2".into(),
            Capability::Illuminance => "illuminance".into(),
            Capability::Contact => "contact".into(),
            Capability::Motion => "motion".into(),
            Capability::Occupancy => "occupancy".into(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
