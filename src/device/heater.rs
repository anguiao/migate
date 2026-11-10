//! Heating-only thermostat values and current-temperature source semantics.

use super::model::PropertyObservation;
use super::{MeasurementSnapshot, TemperatureSourceState};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HeaterMode {
    Off,
    Heat,
}

/// Independent native observations plus the heating-only built-in temperature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeaterSnapshot {
    pub power: PropertyObservation<bool>,
    pub temperature: PropertyObservation<i16>,
    /// Retained for diagnostics even while the source is unavailable.
    pub current_temperature: MeasurementSnapshot,
    /// Whether the built-in source currently allows this reading to be projected.
    pub temperature_source: TemperatureSourceState,
}
impl Default for HeaterSnapshot {
    fn default() -> Self {
        Self {
            power: PropertyObservation::default(),
            temperature: PropertyObservation::default(),
            current_temperature: MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            },
            temperature_source: TemperatureSourceState::Unavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeaterValue {
    Power(bool),
    Temperature(i16),
}
