//! Source-independent thermostat primitives shared by air conditioners and
//! heating-only functions.

use super::{MeasurementSnapshot, TemperatureSourceState, model::PropertyObservation};

/// A native grid in whole degrees Celsius, representable in Matter centidegrees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemperatureGrid {
    min: i16,
    max: i16,
    step: i16,
}
impl TemperatureGrid {
    pub fn new(min: i16, max: i16, step: i16) -> Result<Self, &'static str> {
        if min < -273 || max > 327 || min > max || step <= 0 {
            return Err("invalid thermostat temperature grid");
        }
        Ok(Self { min, max, step })
    }
    pub(crate) fn min(self) -> i16 {
        self.min
    }
    pub(crate) fn max(self) -> i16 {
        self.max
    }
    pub(crate) fn step(self) -> i16 {
        self.step
    }
    pub(crate) fn effective_max(self) -> i16 {
        let min = i32::from(self.min);
        let step = i32::from(self.step);
        (min + (i32::from(self.max) - min) / step * step) as i16
    }
    pub(crate) fn contains(self, value: i16) -> bool {
        (self.min..=self.max).contains(&value)
            && (i32::from(value) - i32::from(self.min)) % i32::from(self.step) == 0
    }
    pub(crate) fn values(self) -> impl Iterator<Item = i16> {
        (i32::from(self.min)..=i32::from(self.max))
            .step_by(self.step as usize)
            .map(|value| value as i16)
    }
}

/// The native observations shared by heating-only functions and air conditioners.
#[derive(Default)]
pub(super) struct ThermostatObservation {
    pub power: PropertyObservation<bool>,
    pub target_temperature: PropertyObservation<i16>,
}

/// One built-in temperature source with the same validity semantics as measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TemperatureSourceRuntime {
    pub measurement: MeasurementSnapshot,
    pub source: TemperatureSourceState,
}
impl Default for TemperatureSourceRuntime {
    fn default() -> Self {
        Self {
            measurement: MeasurementSnapshot {
                value: None,
                source_time: None,
                received_at: None,
            },
            source: TemperatureSourceState::Unavailable,
        }
    }
}
impl TemperatureSourceRuntime {
    pub(super) fn set_available(&mut self, available: bool) -> bool {
        set_temperature_source_available(&mut self.source, available)
    }
    pub(super) fn invalidate(&mut self, source_time: Option<i64>, received_at: i64) -> bool {
        invalidate_temperature_reading(
            &mut self.measurement,
            &mut self.source,
            source_time,
            received_at,
        )
    }
    pub(super) fn observe(
        &mut self,
        value: f64,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        observe_temperature(
            &mut self.measurement,
            &mut self.source,
            value,
            source_time,
            received_at,
        )
    }
}

pub(super) fn set_temperature_source_available(
    source: &mut TemperatureSourceState,
    available: bool,
) -> bool {
    let next = if available {
        match *source {
            TemperatureSourceState::Unavailable => TemperatureSourceState::AwaitingReading,
            current => current,
        }
    } else {
        TemperatureSourceState::Unavailable
    };
    if *source == next {
        false
    } else {
        *source = next;
        true
    }
}

pub(super) fn invalidate_temperature_reading(
    measurement: &mut MeasurementSnapshot,
    source: &mut TemperatureSourceState,
    source_time: Option<i64>,
    received_at: i64,
) -> bool {
    let observed = MeasurementSnapshot {
        value: None,
        source_time,
        received_at: Some(received_at),
    };
    let next = match *source {
        TemperatureSourceState::Available(_) | TemperatureSourceState::AwaitingReading => {
            TemperatureSourceState::AwaitingReading
        }
        other => other,
    };
    if *measurement == observed && *source == next {
        false
    } else {
        *measurement = observed;
        *source = next;
        true
    }
}

pub(super) fn observe_temperature(
    measurement: &mut MeasurementSnapshot,
    source: &mut TemperatureSourceState,
    value: f64,
    source_time: Option<i64>,
    received_at: i64,
) -> bool {
    let observed = MeasurementSnapshot {
        value: Some(value),
        source_time,
        received_at: Some(received_at),
    };
    let next = match *source {
        TemperatureSourceState::AwaitingReading | TemperatureSourceState::Available(_) => {
            TemperatureSourceState::Available(observed)
        }
        other => other,
    };
    if *measurement == observed && *source == next {
        false
    } else {
        *measurement = observed;
        *source = next;
        true
    }
}
