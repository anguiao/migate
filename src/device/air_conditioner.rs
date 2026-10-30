//! Validated air-conditioner values and source-independent control constraints.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AirConditionerMode {
    Cool,
    Heat,
    Auto,
    Fan,
    Dry,
}
impl AirConditionerMode {
    pub(crate) const ALL: [Self; 5] = [Self::Cool, Self::Heat, Self::Auto, Self::Fan, Self::Dry];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Cool => 0,
            Self::Heat => 1,
            Self::Auto => 2,
            Self::Fan => 3,
            Self::Dry => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AirConditionerFanSpeed {
    Auto,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AirConditionerTarget {
    pub mode: Option<AirConditionerMode>,
    pub temperature: Option<i16>,
    pub speed: Option<AirConditionerFanSpeed>,
    pub swing: Option<bool>,
}
impl AirConditionerFanSpeed {
    pub(crate) const ALL: [Self; 4] = [Self::Auto, Self::Low, Self::Medium, Self::High];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Auto => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

/// Independent source observations; command intent never updates these fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AirConditionerSnapshot {
    pub power: super::model::PropertyObservation<bool>,
    pub mode: super::model::PropertyObservation<AirConditionerMode>,
    pub temperature: super::model::PropertyObservation<i16>,
    pub speed: super::model::PropertyObservation<AirConditionerFanSpeed>,
    pub swing: super::model::PropertyObservation<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AirConditionerValue {
    Power(bool),
    Mode(AirConditionerMode),
    Temperature(i16),
    Speed(AirConditionerFanSpeed),
    Swing(bool),
}
impl AirConditionerSnapshot {
    /// The source has already validated the value against its native binding.
    pub(crate) fn update(
        &mut self,
        value: AirConditionerValue,
        source_time: Option<i64>,
        received_at: i64,
    ) -> bool {
        match value {
            AirConditionerValue::Power(value) => self.power.update(value, source_time, received_at),
            AirConditionerValue::Mode(value) => self.mode.update(value, source_time, received_at),
            AirConditionerValue::Temperature(value) => {
                self.temperature.update(value, source_time, received_at)
            }
            AirConditionerValue::Speed(value) => self.speed.update(value, source_time, received_at),
            AirConditionerValue::Swing(value) => self.swing.update(value, source_time, received_at),
        }
    }
}

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
            return Err("invalid air-conditioner temperature grid");
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
    fn values(self) -> impl Iterator<Item = i16> {
        (i32::from(self.min)..=i32::from(self.max))
            .step_by(self.step as usize)
            .map(|value| value as i16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstraintViolation {
    /// The target does not lie on the fixed native temperature grid.
    Constraint,
    /// The current mode does not allow the otherwise valid target.
    InvalidInState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModeConstraints {
    temperatures: Vec<i16>,
    speeds: Vec<AirConditionerFanSpeed>,
}

/// An immutable, validated set of current controls. Replacing this value can
/// represent a new runtime source without changing the fixed native grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AirConditionerConstraints {
    grid: TemperatureGrid,
    modes: [Option<ModeConstraints>; 5],
}
impl AirConditionerConstraints {
    /// Construct source-provided restrictions after intersecting them with the
    /// fixed native grid and semantic speed set at the source boundary.
    pub(crate) fn new(
        grid: TemperatureGrid,
        entries: impl IntoIterator<Item = (AirConditionerMode, Vec<i16>, Vec<AirConditionerFanSpeed>)>,
    ) -> Result<Self, &'static str> {
        let mut modes: [Option<ModeConstraints>; 5] = std::array::from_fn(|_| None);
        for (mode, mut temperatures, mut speeds) in entries {
            if modes[mode.index()].is_some() {
                return Err("duplicate air-conditioner mode");
            }
            if temperatures
                .iter()
                .any(|temperature| !grid.contains(*temperature))
            {
                return Err("air-conditioner constraint temperature is outside the native grid");
            }
            temperatures.sort_unstable();
            if temperatures.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("duplicate air-conditioner constraint temperature");
            }
            speeds.sort_unstable_by_key(|speed| speed.index());
            if speeds.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err("duplicate air-conditioner constraint speed");
            }
            modes[mode.index()] = Some(ModeConstraints {
                temperatures,
                speeds,
            });
        }
        Ok(Self { grid, modes })
    }

    pub(crate) fn temperatures(&self, mode: AirConditionerMode) -> Option<&[i16]> {
        self.modes[mode.index()]
            .as_ref()
            .map(|mode| mode.temperatures.as_slice())
    }

    pub(crate) fn speeds(&self, mode: AirConditionerMode) -> Option<&[AirConditionerFanSpeed]> {
        self.modes[mode.index()]
            .as_ref()
            .map(|mode| mode.speeds.as_slice())
    }

    pub(crate) fn standard(grid: TemperatureGrid) -> Self {
        Self::new(
            grid,
            AirConditionerMode::ALL.map(|mode| {
                (
                    mode,
                    grid.values().collect(),
                    AirConditionerFanSpeed::ALL.to_vec(),
                )
            }),
        )
        .expect("standard constraints use the validated native grid")
    }

    pub(crate) fn grid(&self) -> TemperatureGrid {
        self.grid
    }
    pub(crate) fn validate_mode(
        &self,
        mode: AirConditionerMode,
    ) -> Result<(), ConstraintViolation> {
        self.modes[mode.index()]
            .as_ref()
            .map(|_| ())
            .ok_or(ConstraintViolation::InvalidInState)
    }
    pub(crate) fn validate_temperature(
        &self,
        mode: AirConditionerMode,
        temperature: i16,
    ) -> Result<(), ConstraintViolation> {
        if !self.grid.contains(temperature) {
            return Err(ConstraintViolation::Constraint);
        }
        match &self.modes[mode.index()] {
            Some(constraints) if constraints.temperatures.contains(&temperature) => Ok(()),
            _ => Err(ConstraintViolation::InvalidInState),
        }
    }
    pub(crate) fn validate_speed(
        &self,
        mode: AirConditionerMode,
        speed: AirConditionerFanSpeed,
    ) -> Result<(), ConstraintViolation> {
        match &self.modes[mode.index()] {
            Some(constraints) if constraints.speeds.contains(&speed) => Ok(()),
            _ => Err(ConstraintViolation::InvalidInState),
        }
    }
}

#[cfg(test)]
#[path = "tests/air_conditioner.rs"]
mod tests;
