use std::{error::Error, fmt};

/// A construction error for an invalid integer range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeError {
    Empty,
    ZeroStep,
    StepExceedsSpan,
    InvalidTemperature,
    UnrepresentableTemperature,
}
impl fmt::Display for RangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "range is empty",
            Self::ZeroStep => "range step is zero",
            Self::StepExceedsSpan => "range step exceeds the span",
            Self::InvalidTemperature => "temperature is outside the representable kelvin bounds",
            Self::UnrepresentableTemperature => {
                "temperature range cannot be represented in Matter mireds"
            }
        })
    }
}
impl Error for RangeError {}

/// A native quantization grid. Construction guarantees a non-empty interval
/// and a legal step, so every accepted value lies on its declared grid. The
/// grid belongs to the Xiaomi source boundary; Matter never reads its step.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NumericRange {
    min: u32,
    max: u32,
    step: u32,
}
impl NumericRange {
    pub fn new(min: u32, max: u32, step: u32) -> Result<Self, RangeError> {
        if min >= max {
            return Err(RangeError::Empty);
        }
        if step == 0 {
            return Err(RangeError::ZeroStep);
        }
        if step > max - min {
            return Err(RangeError::StepExceedsSpan);
        }
        Ok(Self { min, max, step })
    }
    pub fn min(self) -> u32 {
        self.min
    }
    pub fn max(self) -> u32 {
        self.max
    }
    pub fn step(self) -> u32 {
        self.step
    }
    pub fn contains(self, value: u32) -> bool {
        (self.min..=self.max).contains(&value) && (value - self.min).is_multiple_of(self.step)
    }
    pub fn effective_max(self) -> u32 {
        self.min + (self.max - self.min) / self.step * self.step
    }
    pub fn nonzero_bounds(self) -> Option<(u32, u32)> {
        let min = if self.min == 0 { self.step } else { self.min };
        let max = self.effective_max();
        (min < max).then_some((min, max))
    }
    /// Clamp and round to the nearest reachable value, without exceeding max.
    /// This is the native source quantization used before a Xiaomi write.
    pub fn quantize(self, value: f64) -> u32 {
        let last = (self.max - self.min) / self.step;
        let index = ((value.clamp(self.min as f64, self.max as f64) - self.min as f64)
            / self.step as f64)
            .round() as u32;
        self.min + index.min(last) * self.step
    }
}

/// A logical color-temperature interval with no quantization step. The native
/// grid keeps its own step in the source binding; group publication and Matter
/// use this interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TemperatureInterval {
    min: u32,
    max: u32,
}
impl TemperatureInterval {
    /// Construct from a native grid's effective span: the declared interval
    /// narrowed to the last reachable grid value. The native grid keeps its own
    /// step for Xiaomi writes; this step-less span is what publication uses.
    pub fn from_grid(range: NumericRange) -> Result<Self, RangeError> {
        Self::new(range.min(), range.effective_max())
    }
    pub fn new(min: u32, max: u32) -> Result<Self, RangeError> {
        if min >= max {
            return Err(RangeError::Empty);
        }
        if min == 0 || max > 1_000_000 {
            return Err(RangeError::InvalidTemperature);
        }
        if mired_span(min, max).is_none() {
            return Err(RangeError::UnrepresentableTemperature);
        }
        Ok(Self { min, max })
    }
    pub fn min(self) -> u32 {
        self.min
    }
    pub fn max(self) -> u32 {
        self.max
    }
    pub fn contains(self, value: u32) -> bool {
        (self.min..=self.max).contains(&value)
    }
}

/// The Matter mired span of a valid temperature range, with its upper bound
/// taken from the interval itself. The interval constructors use this as their
/// private encoding-representability invariant; the Matter boundary projects
/// the same span through `matter::units`.
pub(crate) fn mired_span(min: u32, max: u32) -> Option<(u16, u16)> {
    let lo = 1_000_000_u64.div_ceil(max as u64);
    let hi = 1_000_000_u64 / min as u64;
    (lo > 0 && lo <= hi && hi <= 0xfeff).then_some((lo as u16, hi as u16))
}
