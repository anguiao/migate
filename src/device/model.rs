use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Outlet,
    Light,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    OnOff,
    Brightness,
    ColorTemperature(NumericRange),
    ColorRgb,
}

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

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionSnapshot {
    pub power: Power,
    pub lighting: Option<LightSnapshot>,
    pub availability: Availability,
    pub revision: u64,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FunctionDescription {
    pub id: String,
    pub label: String,
    pub category: Category,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalDeviceDescription {
    pub key: DeviceKey,
    pub label: String,
    pub functions: Vec<FunctionDescription>,
}

/// A confirmed Xiaomi group relation. Members are stable device identities;
/// the value carries no MIoT bindings, commands or Matter endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Composition {
    pub key: DeviceKey,
    pub label: String,
    pub members: Vec<DeviceKey>,
}

/// The account/home that owns a pending or applied publication configuration.
/// Pending entries carry it so the generic Store/DeviceRegistry apply path can
/// commit without holding source context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationScope {
    pub uid: String,
    pub home_id: String,
    pub group_id: String,
}

/// One standalone endpoint of an applied publication set. Group endpoints are
/// carried by their group configuration instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedEndpoint {
    pub key: DeviceKey,
    pub endpoint: u16,
}

/// One group's applied configuration: the confirmed relation together with
/// the capability and range shape derived from its members' real
/// descriptions. The derived shape is persisted with the relation so an
/// applied view survives a member description becoming unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupConfiguration {
    pub key: DeviceKey,
    pub label: String,
    pub members: Vec<DeviceKey>,
    pub capabilities: Vec<Capability>,
}

/// The group shape rule: each member must have the capability, and the
/// common temperature interval is the intersection of the adjustable ranges.
/// Source admission and configuration staging share this rule. Applying a
/// configuration installs the checked shape in the live group.
pub fn derive_group_capabilities<'a>(
    functions: impl IntoIterator<Item = &'a FunctionDescription>,
) -> Vec<Capability> {
    let functions = functions.into_iter().collect::<Vec<_>>();
    let mut capabilities = Vec::new();
    if functions
        .iter()
        .all(|function| function.capabilities.contains(&Capability::OnOff))
    {
        capabilities.push(Capability::OnOff);
    }
    if functions
        .iter()
        .all(|function| function.capabilities.contains(&Capability::Brightness))
    {
        capabilities.push(Capability::Brightness);
    }
    if let Some(range) = common_temperature(&functions) {
        capabilities.push(Capability::ColorTemperature(range));
    }
    if functions
        .iter()
        .all(|function| function.capabilities.contains(&Capability::ColorRgb))
    {
        capabilities.push(Capability::ColorRgb);
    }
    capabilities
}

fn common_temperature(functions: &[&FunctionDescription]) -> Option<NumericRange> {
    let first = functions.first()?.temperature_range()?;
    let mut min = first.min;
    let mut max = first.effective_max();
    for function in &functions[1..] {
        let range = function.temperature_range()?;
        min = min.max(range.min);
        max = max.min(range.effective_max());
    }
    (min < max).then_some(NumericRange { min, max, step: 1 })
}

/// An integer physical range. Every accepted value lies on its declared grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NumericRange {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}
impl NumericRange {
    pub fn valid(self) -> bool {
        self.min < self.max && self.step > 0 && self.step <= self.max - self.min
    }
    pub fn contains(self, value: u32) -> bool {
        self.valid()
            && (self.min..=self.max).contains(&value)
            && (value - self.min).is_multiple_of(self.step)
    }
    pub fn effective_max(self) -> u32 {
        self.min + (self.max - self.min) / self.step * self.step
    }
    pub fn nonzero_bounds(self) -> Option<(u32, u32)> {
        if !self.valid() {
            return None;
        }
        let min = if self.min == 0 { self.step } else { self.min };
        let max = self.effective_max();
        (min < max).then_some((min, max))
    }
    /// Clamp and round to the nearest reachable value, without exceeding max.
    pub fn quantize(self, value: f64) -> u32 {
        let last = (self.max - self.min) / self.step;
        let index = ((value.clamp(self.min as f64, self.max as f64) - self.min as f64)
            / self.step as f64)
            .round() as u32;
        self.min + index.min(last) * self.step
    }
    pub fn mired_bounds(self) -> Result<(u16, u16), CommandError> {
        if !self.valid() || self.min == 0 {
            return Err(CommandError::Rejected("invalid temperature range".into()));
        }
        // Check the full physical range before narrowing to the Matter encoding.
        let min = 1_000_000_u64.div_ceil(self.effective_max() as u64);
        let max = 1_000_000_u64 / self.min as u64;
        if min == 0 || min > max || max > 0xfeff {
            return Err(CommandError::Rejected(
                "temperature range cannot be represented in Matter mireds".into(),
            ));
        }
        Ok((min as u16, max as u16))
    }
    pub fn kelvin_from_mireds(self, mireds: u16) -> Result<u32, CommandError> {
        self.mired_bounds()?;
        if !(1..=0xfeff).contains(&mireds) {
            return Err(CommandError::Rejected(
                "color temperature is outside the Matter range".into(),
            ));
        }
        Ok(self.quantize(1_000_000.0 / mireds as f64))
    }
    pub fn mireds_from_kelvin(self, kelvin: u32) -> Result<u16, CommandError> {
        if !self.contains(kelvin) {
            return Err(CommandError::Rejected(
                "color temperature is outside its physical range".into(),
            ));
        }
        let (min, max) = self.mired_bounds()?;
        Ok(((1_000_000.0 / kelvin as f64).round() as u16).clamp(min, max))
    }
}

/// Nonzero brightness uses [1, 100] percent; zero is handled by command semantics.
pub fn brightness_from_matter_level(level: u8) -> Result<f64, CommandError> {
    if !(1..=254).contains(&level) {
        return Err(CommandError::Rejected(
            "brightness level is outside 1..254".into(),
        ));
    }
    Ok(1.0 + (level as f64 - 1.0) * 99.0 / 253.0)
}
pub fn brightness_to_matter_level(percent: f64) -> Result<u8, CommandError> {
    validate_brightness(percent)?;
    Ok((1.0 + (percent - 1.0) * 253.0 / 99.0).round() as u8)
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
/// Observations from a reachable, open function. Unavailable or closed
/// functions expose unknown values; structural color modes remain known.
#[derive(Clone, Debug, PartialEq)]
pub struct FreshSnapshot {
    pub key: FunctionKey,
    pub snapshot: FunctionSnapshot,
    pub reachable: bool,
}
impl Capability {
    fn index(self) -> usize {
        match self {
            Self::OnOff => 0,
            Self::Brightness => 1,
            Self::ColorTemperature(_) => 2,
            Self::ColorRgb => 3,
        }
    }

    /// Physical well-formedness only; Matter representability belongs to the
    /// publication mapping and does not restrict the generic model.
    fn well_formed(self) -> bool {
        match self {
            Self::ColorTemperature(range) => {
                range.valid() && range.min > 0 && range.max <= 1_000_000
            }
            Self::OnOff | Self::Brightness | Self::ColorRgb => true,
        }
    }
}

/// Light capabilities are independent: any nonempty subset of the four kinds
/// is valid, each kind appears at most once, and declared temperature ranges
/// only need to be physically well formed.
fn valid_light_capabilities(capabilities: &[Capability]) -> bool {
    if capabilities.is_empty() {
        return false;
    }
    let mut seen = [false; 4];
    for capability in capabilities {
        if !capability.well_formed() {
            return false;
        }
        if std::mem::replace(&mut seen[capability.index()], true) {
            return false;
        }
    }
    true
}

impl FunctionDescription {
    pub fn temperature_range(&self) -> Option<NumericRange> {
        self.capabilities
            .iter()
            .find_map(|capability| match capability {
                Capability::ColorTemperature(range) => Some(*range),
                _ => None,
            })
    }
    /// Capability lists are semantically equal when they contain the same
    /// definite values regardless of order. Descriptions are validated, so a
    /// kind appears at most once and value equality is enough.
    pub fn same_capabilities(&self, other: &Self) -> bool {
        self.capabilities.len() == other.capabilities.len()
            && self
                .capabilities
                .iter()
                .all(|capability| other.capabilities.contains(capability))
    }
    pub fn valid_capabilities(&self) -> bool {
        match self.category {
            Category::Outlet => self.capabilities == [Capability::OnOff],
            Category::Light => valid_light_capabilities(&self.capabilities),
        }
    }
}
