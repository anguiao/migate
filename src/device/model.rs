mod light;
mod range;

pub(crate) use light::validate_brightness;
pub use light::{LightMode, LightPatch, LightProfile, LightProfileKind, LightSnapshot, Rgb};
pub(crate) use range::mired_span;
pub use range::{NumericRange, RangeError, TemperatureInterval};

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
    Curtain,
    Fan,
    AirConditioner,
    Brightness,
    ColorTemperature(TemperatureInterval),
    ColorRgb,
    Temperature,
    Humidity,
    CarbonDioxide,
    Illuminance,
    Contact,
    Motion,
    Occupancy,
}

/// The closed set of function kinds; every function is one of these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunctionKind {
    Outlet,
    Light(LightProfile),
    Curtain,
    Fan {
        horizontal_swing: bool,
        speed_count: u8,
    },
    AirConditioner {
        temperature: super::TemperatureGrid,
        vertical_swing: bool,
    },
    Temperature(MeasurementRange),
    Humidity(MeasurementRange),
    CarbonDioxide(MeasurementRange),
    Illuminance(MeasurementRange),
    Contact,
    Motion,
    Occupancy,
}
impl FunctionKind {
    pub fn is_control(self) -> bool {
        matches!(
            self,
            Self::Outlet
                | Self::Light(_)
                | Self::Curtain
                | Self::Fan { .. }
                | Self::AirConditioner { .. }
        )
    }
    pub fn is_state_sensor(self) -> bool {
        matches!(self, Self::Contact | Self::Motion | Self::Occupancy)
    }
    pub fn is_read_only(self) -> bool {
        !self.is_control()
    }
    pub fn is_light(self) -> bool {
        matches!(self, Self::Light(_))
    }
    pub fn light_profile(self) -> Option<LightProfile> {
        match self {
            Self::Light(profile) => Some(profile),
            Self::Outlet
            | Self::Curtain
            | Self::Fan { .. }
            | Self::AirConditioner { .. }
            | Self::Temperature(_)
            | Self::Humidity(_)
            | Self::CarbonDioxide(_)
            | Self::Illuminance(_)
            | Self::Contact
            | Self::Motion
            | Self::Occupancy => None,
        }
    }
    pub fn is_measurement(self) -> bool {
        matches!(
            self,
            Self::Temperature(_)
                | Self::Humidity(_)
                | Self::CarbonDioxide(_)
                | Self::Illuminance(_)
        )
    }
    /// Derived only for diagnostics.
    pub fn capabilities(self) -> Vec<Capability> {
        match self {
            Self::Temperature(_) => vec![Capability::Temperature],
            Self::Humidity(_) => vec![Capability::Humidity],
            Self::CarbonDioxide(_) => vec![Capability::CarbonDioxide],
            Self::Illuminance(_) => vec![Capability::Illuminance],
            Self::Contact => vec![Capability::Contact],
            Self::Motion => vec![Capability::Motion],
            Self::Occupancy => vec![Capability::Occupancy],
            Self::Outlet => vec![Capability::OnOff],
            Self::Curtain => vec![Capability::Curtain],
            Self::Fan { .. } => vec![Capability::OnOff, Capability::Fan],
            Self::AirConditioner { .. } => vec![Capability::OnOff, Capability::AirConditioner],
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
        if functions.iter().any(|function| {
            matches!(
                function.kind,
                FunctionKind::Fan {
                    speed_count: 0 | 101..=u8::MAX,
                    ..
                }
            )
        }) {
            return Err(DescriptionError("fan speed count must be in 1..=100"));
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
    pub(crate) fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }
    /// Update a selected light without changing its function identity.
    pub(crate) fn with_light_profile(mut self, id: &str, profile: LightProfile) -> Self {
        let function = self
            .functions
            .iter_mut()
            .find(|function| function.id == id)
            .unwrap();
        debug_assert!(function.kind.is_light());
        function.kind = FunctionKind::Light(profile);
        self
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

/// Current eligibility of a temperature function as an ambient source.
/// `Available` contains an observation obtained while a path was valid;
/// the ordinary measurement snapshot can retain an older diagnostic value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemperatureSourceState {
    Unavailable,
    AwaitingReading,
    Available(MeasurementSnapshot),
}

/// An air conditioner's current ambient temperature projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AmbientTemperature {
    Unbound,
    SourceMissing,
    SourceUnavailable,
    AwaitingReading,
    Available(MeasurementSnapshot),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CurtainMotion {
    Opening,
    Closing,
    Stopped,
}

/// One validated native curtain report.
pub(crate) enum CurtainValue {
    CurrentPosition(u32),
    TargetPosition(u32),
    Status(CurtainMotion),
}

/// A native property and its independent observation metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyObservation<T> {
    pub value: Option<T>,
    pub source_time: Option<i64>,
    pub received_at: Option<i64>,
}

impl<T> Default for PropertyObservation<T> {
    fn default() -> Self {
        Self {
            value: None,
            source_time: None,
            received_at: None,
        }
    }
}
impl<T: PartialEq> PropertyObservation<T> {
    pub(super) fn update(&mut self, value: T, source_time: Option<i64>, received_at: i64) -> bool {
        let changed = self.value.as_ref() != Some(&value);
        *self = Self {
            value: Some(value),
            source_time,
            received_at: Some(received_at),
        };
        changed
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CurtainSnapshot {
    pub current_position: PropertyObservation<u32>,
    pub target_position: PropertyObservation<u32>,
    pub status: PropertyObservation<CurtainMotion>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FanSnapshot {
    pub power: PropertyObservation<bool>,
    pub speed: PropertyObservation<u8>,
    pub swing: PropertyObservation<bool>,
}
pub(crate) enum FanValue {
    Power(bool),
    Speed(u8),
    Swing(bool),
}

/// The native input participating in the latest binary-state observation.
#[derive(Clone, Debug, PartialEq)]
pub enum StateInput {
    Property(serde_json::Value),
    Event { siid: u32, eiid: u32 },
}

/// Active means open, motion detected, or occupied for the respective kind.
#[derive(Clone, Debug, PartialEq)]
pub struct StateSnapshot {
    pub active: Option<bool>,
    pub input: Option<StateInput>,
    pub source_time: Option<i64>,
    pub received_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionSnapshot {
    pub state: FunctionState,
    pub availability: Availability,
    pub revision: u64,
    pub(super) temperature_source: Option<TemperatureSourceState>,
    pub(super) ambient_temperature: Option<AmbientTemperature>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FunctionState {
    Outlet {
        power: Power,
    },
    Light {
        power: Power,
        lighting: LightSnapshot,
    },
    Curtain {
        curtain: CurtainSnapshot,
    },
    Fan {
        fan: FanSnapshot,
    },
    AirConditioner {
        air_conditioner: super::AirConditionerSnapshot,
    },
    Temperature {
        measurement: MeasurementSnapshot,
    },
    Humidity {
        measurement: MeasurementSnapshot,
    },
    CarbonDioxide {
        measurement: MeasurementSnapshot,
    },
    Illuminance {
        measurement: MeasurementSnapshot,
    },
    Contact {
        state: StateSnapshot,
    },
    Motion {
        state: StateSnapshot,
    },
    Occupancy {
        state: StateSnapshot,
    },
}
impl FunctionSnapshot {
    /// Return the temperature source state, or `None` for other functions.
    pub fn temperature_source(&self) -> Option<TemperatureSourceState> {
        self.temperature_source
    }
    /// Ambient temperature is independent of the air-conditioner setpoint.
    pub fn ambient_temperature(&self) -> Option<AmbientTemperature> {
        self.ambient_temperature
    }
    pub fn air_conditioner(&self) -> Option<super::AirConditionerSnapshot> {
        match self.state {
            FunctionState::AirConditioner { air_conditioner } => Some(air_conditioner),
            _ => None,
        }
    }
    pub fn fan(&self) -> Option<FanSnapshot> {
        match self.state {
            FunctionState::Fan { fan } => Some(fan),
            _ => None,
        }
    }

    pub fn state_sensor(&self) -> Option<StateSnapshot> {
        match &self.state {
            FunctionState::Contact { state }
            | FunctionState::Motion { state }
            | FunctionState::Occupancy { state } => Some(state.clone()),
            _ => None,
        }
    }
    pub fn power(&self) -> Option<Power> {
        match &self.state {
            FunctionState::Outlet { power } | FunctionState::Light { power, .. } => Some(*power),
            FunctionState::Fan { fan } => {
                Some(fan.power.value.map(Power::from).unwrap_or(Power::Unknown))
            }
            FunctionState::AirConditioner { air_conditioner } => Some(
                air_conditioner
                    .power
                    .value
                    .map(Power::from)
                    .unwrap_or(Power::Unknown),
            ),
            FunctionState::Temperature { .. }
            | FunctionState::Curtain { .. }
            | FunctionState::Humidity { .. }
            | FunctionState::CarbonDioxide { .. }
            | FunctionState::Illuminance { .. }
            | FunctionState::Contact { .. }
            | FunctionState::Motion { .. }
            | FunctionState::Occupancy { .. } => None,
        }
    }
    pub fn lighting(&self) -> Option<LightSnapshot> {
        match &self.state {
            FunctionState::Light { lighting, .. } => Some(*lighting),
            FunctionState::Outlet { .. }
            | FunctionState::Curtain { .. }
            | FunctionState::Fan { .. }
            | FunctionState::AirConditioner { .. }
            | FunctionState::Temperature { .. }
            | FunctionState::Humidity { .. }
            | FunctionState::CarbonDioxide { .. }
            | FunctionState::Illuminance { .. }
            | FunctionState::Contact { .. }
            | FunctionState::Motion { .. }
            | FunctionState::Occupancy { .. } => None,
        }
    }
    pub fn measurement(&self) -> Option<MeasurementSnapshot> {
        match &self.state {
            FunctionState::Temperature { measurement }
            | FunctionState::Humidity { measurement }
            | FunctionState::CarbonDioxide { measurement }
            | FunctionState::Illuminance { measurement } => Some(*measurement),
            FunctionState::Outlet { .. }
            | FunctionState::Light { .. }
            | FunctionState::Curtain { .. }
            | FunctionState::Fan { .. }
            | FunctionState::AirConditioner { .. }
            | FunctionState::Contact { .. }
            | FunctionState::Motion { .. }
            | FunctionState::Occupancy { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unsupported,
    Unavailable(String),
    UnknownState,
    Constraint,
    InvalidInState,
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
            Self::Constraint => formatter.write_str("value is outside the supported constraint"),
            Self::InvalidInState => {
                formatter.write_str("value is not allowed in the current state")
            }
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[cfg(test)]
#[path = "tests/model_access.rs"]
mod model_access;
