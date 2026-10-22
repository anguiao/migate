use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceKey {
    pub source: String,
    pub id: String,
}

impl DeviceKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
        }
    }
}

/// Diagnostics-only capability list derived from a valid function kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    OnOff,
    Brightness,
    ColorTemperature(TemperatureInterval),
    ColorRgb,
    Temperature,
    Humidity,
}

/// The four standard light profiles without their interval, for comparisons
/// that treat equivalent logical intervals independently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightProfileKind {
    OnOff,
    Dimmable,
    ColorTemperature,
    ExtendedColor,
}

/// The four standard light profiles. Color-temperature profiles carry their
/// valid logical interval; the extended profile additionally implies RGB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightProfile {
    OnOff,
    Dimmable,
    ColorTemperature(TemperatureInterval),
    ExtendedColor(TemperatureInterval),
}
impl LightProfile {
    pub fn kind(self) -> LightProfileKind {
        match self {
            Self::OnOff => LightProfileKind::OnOff,
            Self::Dimmable => LightProfileKind::Dimmable,
            Self::ColorTemperature(_) => LightProfileKind::ColorTemperature,
            Self::ExtendedColor(_) => LightProfileKind::ExtendedColor,
        }
    }
    pub fn temperature_interval(self) -> Option<TemperatureInterval> {
        match self {
            Self::ColorTemperature(interval) | Self::ExtendedColor(interval) => Some(interval),
            Self::OnOff | Self::Dimmable => None,
        }
    }
    pub fn has_brightness(self) -> bool {
        !matches!(self, Self::OnOff)
    }
    pub fn has_rgb(self) -> bool {
        matches!(self, Self::ExtendedColor(_))
    }
    /// A color temperature profile determines its mode; extended color needs feedback.
    pub fn inferred_color_mode(self) -> Option<LightMode> {
        match self {
            Self::ColorTemperature(_) => Some(LightMode::Temperature),
            Self::OnOff | Self::Dimmable | Self::ExtendedColor(_) => None,
        }
    }
}

/// The closed set of function kinds; every function is one of these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunctionKind {
    Outlet,
    Light(LightProfile),
    Temperature(MeasurementRange),
    Humidity(MeasurementRange),
}
impl FunctionKind {
    pub fn is_light(self) -> bool {
        matches!(self, Self::Light(_))
    }
    pub fn light_profile(self) -> Option<LightProfile> {
        match self {
            Self::Light(profile) => Some(profile),
            Self::Outlet | Self::Temperature(_) | Self::Humidity(_) => None,
        }
    }
    pub fn is_measurement(self) -> bool {
        matches!(self, Self::Temperature(_) | Self::Humidity(_))
    }
    pub fn measurement_range(self) -> Option<MeasurementRange> {
        match self {
            Self::Temperature(range) | Self::Humidity(range) => Some(range),
            Self::Outlet | Self::Light(_) => None,
        }
    }
    /// Derived only for diagnostics.
    pub fn capabilities(self) -> Vec<Capability> {
        match self {
            Self::Temperature(_) => vec![Capability::Temperature],
            Self::Humidity(_) => vec![Capability::Humidity],
            Self::Outlet => vec![Capability::OnOff],
            Self::Light(profile) => {
                let mut capabilities = vec![Capability::OnOff];
                if profile.has_brightness() {
                    capabilities.push(Capability::Brightness);
                }
                if let Some(interval) = profile.temperature_interval() {
                    capabilities.push(Capability::ColorTemperature(interval));
                }
                if profile.has_rgb() {
                    capabilities.push(Capability::ColorRgb);
                }
                capabilities
            }
        }
    }
}

/// A function's stable identity and its valid kind. The kind is closed, so
/// category and profile cannot be changed independently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionDescription {
    pub id: String,
    pub label: String,
    pub kind: FunctionKind,
}
impl FunctionDescription {
    /// Derived for diagnostics only.
    pub fn capabilities(&self) -> Vec<Capability> {
        self.kind.capabilities()
    }
    pub fn temperature_interval(&self) -> Option<TemperatureInterval> {
        self.kind
            .light_profile()
            .and_then(LightProfile::temperature_interval)
    }
    pub fn supports_brightness(&self) -> bool {
        self.kind
            .light_profile()
            .is_some_and(LightProfile::has_brightness)
    }
    pub fn supports_color_temperature(&self) -> bool {
        self.temperature_interval().is_some()
    }
    pub fn supports_rgb(&self) -> bool {
        self.kind.light_profile().is_some_and(LightProfile::has_rgb)
    }
    pub fn inferred_color_mode(&self) -> Option<LightMode> {
        self.kind
            .light_profile()
            .and_then(LightProfile::inferred_color_mode)
    }
    pub fn is_measurement(&self) -> bool {
        self.kind.is_measurement()
    }
    pub fn is_read_only(&self) -> bool {
        self.is_measurement()
    }
    pub fn measurement_range(&self) -> Option<MeasurementRange> {
        self.kind.measurement_range()
    }
}

/// A physical device description with non-empty, uniquely keyed functions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalDeviceDescription {
    key: DeviceKey,
    label: String,
    functions: Vec<FunctionDescription>,
    battery: bool,
}
impl PhysicalDeviceDescription {
    pub fn new(
        key: DeviceKey,
        label: impl Into<String>,
        functions: Vec<FunctionDescription>,
    ) -> Result<Self, DescriptionError> {
        if functions.is_empty() {
            return Err(DescriptionError(
                "a device description needs at least one function",
            ));
        }
        let mut ids = std::collections::HashSet::new();
        if functions
            .iter()
            .any(|function| !ids.insert(function.id.as_str()))
        {
            return Err(DescriptionError("function keys must be unique"));
        }
        Ok(Self {
            key,
            label: label.into(),
            functions,
            battery: false,
        })
    }
    /// Mark the physical device as exposing one valid shared battery capability.
    pub fn with_battery(mut self) -> Self {
        self.battery = true;
        self
    }
    pub fn has_battery(&self) -> bool {
        self.battery
    }
    pub fn functions(&self) -> &[FunctionDescription] {
        &self.functions
    }
    pub(crate) fn into_parts(self) -> (DeviceKey, String, Vec<FunctionDescription>, bool) {
        (self.key, self.label, self.functions, self.battery)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptionError(&'static str);
impl fmt::Display for DescriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}
impl Error for DescriptionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Power {
    Unknown,
    On,
    Off,
}

impl Power {
    pub fn value(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }
}

impl From<bool> for Power {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    Initializing,
    Available,
    Unavailable(String),
}

impl Availability {
    pub(super) fn command_result(&self) -> Result<(), CommandError> {
        match self {
            Self::Available => Ok(()),
            Self::Initializing => Err(CommandError::Unavailable("initializing".into())),
            Self::Unavailable(reason) => Err(CommandError::Unavailable(reason.clone())),
        }
    }
}

/// A finite native measurement interval. Values are in the source unit.
#[derive(Clone, Copy, Debug)]
pub struct MeasurementRange {
    min: f64,
    max: f64,
}
impl MeasurementRange {
    pub fn new(min: f64, max: f64) -> Result<Self, RangeError> {
        if !min.is_finite() || !max.is_finite() || min >= max {
            return Err(RangeError::Empty);
        }
        Ok(Self { min, max })
    }
    pub fn min(self) -> f64 {
        self.min
    }
    pub fn max(self) -> f64 {
        self.max
    }
    pub fn contains(self, value: f64) -> bool {
        value.is_finite() && (self.min..=self.max).contains(&value)
    }
    /// The standard Matter temperature bounds after the native unit is
    /// projected by 100. Both the raw and rounded bounds must fit.
    pub(crate) fn temperature_bounds(self) -> Option<(i16, i16)> {
        const MIN_NATIVE: f64 = -273.15;
        const MAX_NATIVE: f64 = 327.67;
        const MIN: i64 = -27_315;
        const MIN_UPPER: i64 = 32_766;
        const MAX_UPPER: i64 = 32_767;
        if self.min < MIN_NATIVE || self.max > MAX_NATIVE {
            return None;
        }
        let (lo, hi) = (self.min * 100.0, self.max * 100.0);
        if lo >= hi {
            return None;
        }
        let (lo, hi) = (lo.round() as i64, hi.round() as i64);
        ((MIN..=MIN_UPPER).contains(&lo) && (-27_314..=MAX_UPPER).contains(&hi) && lo < hi)
            .then_some((lo as i16, hi as i16))
    }
    /// The relative-humidity limits are 0..=10000, with MinMeasuredValue at
    /// most 9999 and MaxMeasuredValue at least 1.
    pub(crate) fn humidity_bounds(self) -> Option<(u16, u16)> {
        if self.min < 0.0 || self.max > 100.0 {
            return None;
        }
        let (lo, hi) = (self.min * 100.0, self.max * 100.0);
        if lo >= hi {
            return None;
        }
        let (lo, hi) = (lo.round() as i64, hi.round() as i64);
        (lo <= 9_999 && (1..=10_000).contains(&hi) && lo < hi).then_some((lo as u16, hi as u16))
    }
}
impl PartialEq for MeasurementRange {
    fn eq(&self, other: &Self) -> bool {
        self.min.to_bits() == other.min.to_bits() && self.max.to_bits() == other.max.to_bits()
    }
}
impl Eq for MeasurementRange {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BatterySnapshot {
    pub value: Option<f64>,
    pub source_time: Option<i64>,
    pub received_at: Option<i64>,
}
impl Eq for BatterySnapshot {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasurementSnapshot {
    pub value: Option<f64>,
    pub source_time: Option<i64>,
    pub received_at: Option<i64>,
}
impl Eq for MeasurementSnapshot {}

#[derive(Clone, Debug, PartialEq)]
pub enum FunctionSnapshot {
    Outlet {
        power: Power,
        availability: Availability,
        revision: u64,
    },
    Light {
        power: Power,
        lighting: LightSnapshot,
        availability: Availability,
        revision: u64,
    },
    Temperature {
        measurement: MeasurementSnapshot,
        availability: Availability,
        revision: u64,
    },
    Humidity {
        measurement: MeasurementSnapshot,
        availability: Availability,
        revision: u64,
    },
}
impl FunctionSnapshot {
    pub fn power(&self) -> Option<Power> {
        match self {
            Self::Outlet { power, .. } | Self::Light { power, .. } => Some(*power),
            Self::Temperature { .. } | Self::Humidity { .. } => None,
        }
    }
    pub fn lighting(&self) -> Option<LightSnapshot> {
        match self {
            Self::Light { lighting, .. } => Some(*lighting),
            Self::Outlet { .. } | Self::Temperature { .. } | Self::Humidity { .. } => None,
        }
    }
    pub fn measurement(&self) -> Option<MeasurementSnapshot> {
        match self {
            Self::Temperature { measurement, .. } | Self::Humidity { measurement, .. } => {
                Some(*measurement)
            }
            Self::Outlet { .. } | Self::Light { .. } => None,
        }
    }
    pub fn availability(&self) -> Availability {
        match self {
            Self::Outlet { availability, .. }
            | Self::Light { availability, .. }
            | Self::Temperature { availability, .. }
            | Self::Humidity { availability, .. } => availability.clone(),
        }
    }
    pub fn revision(&self) -> u64 {
        match self {
            Self::Outlet { revision, .. }
            | Self::Light { revision, .. }
            | Self::Temperature { revision, .. }
            | Self::Humidity { revision, .. } => *revision,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unsupported,
    Unavailable(String),
    UnknownState,
    Timeout,
    Rejected(String),
    SendFailed(String),
    Cancelled,
    Superseded,
    Stopped,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("device does not support this command"),
            Self::Unavailable(reason) => write!(formatter, "device unavailable: {reason}"),
            Self::UnknownState => formatter.write_str("required device state is unknown"),
            Self::Timeout => formatter.write_str("command acceptance timed out"),
            Self::Rejected(reason) => write!(formatter, "command rejected: {reason}"),
            Self::SendFailed(reason) => write!(formatter, "command send failed: {reason}"),
            Self::Cancelled => formatter.write_str("command cancelled"),
            Self::Superseded => formatter.write_str("command superseded"),
            Self::Stopped => formatter.write_str("action stopped"),
        }
    }
}

impl Error for CommandError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FunctionKey {
    pub device: DeviceKey,
    pub id: String,
}

impl FunctionKey {
    pub fn new(device: DeviceKey, id: impl Into<String>) -> Self {
        Self {
            device,
            id: id.into(),
        }
    }
}

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

pub(crate) fn validate_brightness(percent: f64) -> Result<(), CommandError> {
    if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
        return Err(CommandError::Rejected(
            "brightness is outside 1..100 percent".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}
impl Rgb {
    pub fn from_packed(value: u32) -> Option<Self> {
        (value <= 0xffffff).then_some(Self {
            red: (value >> 16) as u8,
            green: (value >> 8) as u8,
            blue: value as u8,
        })
    }
    pub fn packed(self) -> u32 {
        (self.red as u32) << 16 | (self.green as u32) << 8 | self.blue as u32
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightMode {
    Temperature,
    Rgb,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightSnapshot {
    pub brightness_percent: Option<f64>,
    pub temperature_kelvin: Option<u32>,
    pub color: Option<Rgb>,
    pub mode: Option<LightMode>,
}
/// A single accepted report; omitted properties preserve existing observations.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LightPatch {
    pub power: Option<bool>,
    pub brightness_percent: Option<f64>,
    pub temperature_kelvin: Option<u32>,
    pub color: Option<Rgb>,
    pub mode: Option<LightMode>,
}

#[cfg(test)]
#[path = "tests/model_access.rs"]
mod model_access;
