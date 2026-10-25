use std::{error::Error, fmt};

use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, LightMode, LightProfile, MeasurementRange,
    NumericRange, PhysicalDeviceDescription, TemperatureInterval,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PropertyBinding {
    pub siid: u32,
    pub piid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedFunction {
    id: String,
    label: String,
    bindings: FunctionBindings,
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum FunctionBindings {
    Outlet {
        power: PropertyBinding,
    },
    Light {
        power: PropertyBinding,
        lighting: LightBindings,
    },
    Temperature {
        measurement: MeasurementBinding,
    },
    Humidity {
        measurement: MeasurementBinding,
    },
    CarbonDioxide {
        measurement: MeasurementBinding,
    },
}
impl ParsedFunction {
    pub(crate) fn outlet(
        id: impl Into<String>,
        label: impl Into<String>,
        power: PropertyBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Outlet { power },
        }
    }
    pub(crate) fn light(
        id: impl Into<String>,
        label: impl Into<String>,
        power: PropertyBinding,
        lighting: LightBindings,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Light { power, lighting },
        }
    }
    pub(crate) fn temperature(
        id: impl Into<String>,
        label: impl Into<String>,
        measurement: MeasurementBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Temperature { measurement },
        }
    }
    pub(crate) fn humidity(
        id: impl Into<String>,
        label: impl Into<String>,
        measurement: MeasurementBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Humidity { measurement },
        }
    }
    pub(crate) fn co2(
        id: impl Into<String>,
        label: impl Into<String>,
        measurement: MeasurementBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::CarbonDioxide { measurement },
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn power(&self) -> Option<PropertyBinding> {
        match &self.bindings {
            FunctionBindings::Outlet { power } | FunctionBindings::Light { power, .. } => {
                Some(*power)
            }
            FunctionBindings::Temperature { .. }
            | FunctionBindings::Humidity { .. }
            | FunctionBindings::CarbonDioxide { .. } => None,
        }
    }
    pub fn lighting(&self) -> Option<&LightBindings> {
        match &self.bindings {
            FunctionBindings::Outlet { .. }
            | FunctionBindings::Temperature { .. }
            | FunctionBindings::Humidity { .. }
            | FunctionBindings::CarbonDioxide { .. } => None,
            FunctionBindings::Light { lighting, .. } => Some(lighting),
        }
    }
    pub fn measurement(&self) -> Option<&MeasurementBinding> {
        match &self.bindings {
            FunctionBindings::Temperature { measurement }
            | FunctionBindings::Humidity { measurement }
            | FunctionBindings::CarbonDioxide { measurement } => Some(measurement),
            FunctionBindings::Outlet { .. } | FunctionBindings::Light { .. } => None,
        }
    }
    pub fn kind(&self) -> FunctionKind {
        match &self.bindings {
            FunctionBindings::Outlet { .. } => FunctionKind::Outlet,
            FunctionBindings::Light { lighting, .. } => FunctionKind::Light(lighting.profile()),
            FunctionBindings::Temperature { measurement } => {
                FunctionKind::Temperature(measurement.range())
            }
            FunctionBindings::Humidity { measurement } => {
                FunctionKind::Humidity(measurement.range())
            }
            FunctionBindings::CarbonDioxide { measurement } => {
                FunctionKind::CarbonDioxide(measurement.range())
            }
        }
    }
    /// The logical description derived from the real bindings on demand.
    pub fn description(&self) -> FunctionDescription {
        FunctionDescription {
            id: self.id.clone(),
            label: self.label.clone(),
            kind: self.kind(),
        }
    }
    /// Every native binding this function really reads or writes.
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        if let Some(measurement) = self.measurement() {
            return vec![measurement.property()];
        }
        let mut bindings = vec![self.power().expect("control functions have power")];
        if let Some(lighting) = self.lighting() {
            bindings.extend(lighting.bindings());
        }
        bindings
    }
}

/// A parsed device description with non-empty, uniquely keyed functions and
/// uniquely owned real bindings. Lights use the single `main` function layout.
#[derive(Clone, Debug)]
pub struct ParsedDevice {
    key: DeviceKey,
    functions: Vec<ParsedFunction>,
    refresh_trigger: Option<PropertyBinding>,
    battery: Option<BatteryBinding>,
    battery_issue: Option<String>,
}
impl PartialEq for ParsedDevice {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
            && self.functions == other.functions
            && self.refresh_trigger == other.refresh_trigger
            && self.battery == other.battery
    }
}
impl Eq for ParsedDevice {}
impl ParsedDevice {
    pub(crate) fn new(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::new_with_battery(key, functions, refresh_trigger, None)
    }
    pub(crate) fn new_with_battery(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
    ) -> Result<Self, CapabilityError> {
        Self::build(key, functions, refresh_trigger, battery, None)
    }
    pub(crate) fn new_with_battery_diagnostic(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
        battery_issue: Option<String>,
    ) -> Result<Self, CapabilityError> {
        Self::build(key, functions, refresh_trigger, battery, battery_issue)
    }
    fn build(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
        battery: Option<BatteryBinding>,
        battery_issue: Option<String>,
    ) -> Result<Self, CapabilityError> {
        if functions.is_empty() {
            return Err(CapabilityError::InvalidDescription("functions"));
        }
        if battery_issue
            .as_ref()
            .is_some_and(|issue| issue.is_empty() || issue.chars().any(char::is_control))
            || (battery.is_some() && battery_issue.is_some())
        {
            return Err(CapabilityError::InvalidDescription("battery diagnostic"));
        }
        let mut ids = std::collections::HashSet::new();
        if functions.iter().any(|function| !ids.insert(function.id())) {
            return Err(CapabilityError::InvalidDescription(
                "duplicate function key",
            ));
        }
        let mut bindings = std::collections::HashSet::new();
        for binding in functions
            .iter()
            .flat_map(ParsedFunction::bindings)
            .chain(refresh_trigger)
            .chain(battery.as_ref().map(BatteryBinding::property))
        {
            if binding.siid == 0 || binding.piid == 0 || !bindings.insert(binding) {
                return Err(CapabilityError::InvalidDescription("function bindings"));
            }
        }
        let controls = functions
            .iter()
            .filter(|function| function.measurement().is_none())
            .collect::<Vec<_>>();
        let measurements = functions
            .iter()
            .filter(|function| function.measurement().is_some())
            .collect::<Vec<_>>();
        for function in &measurements {
            let measurement = function.measurement().unwrap();
            if !matches!(
                (function.id(), function.kind(), measurement.unit()),
                (
                    "temperature",
                    FunctionKind::Temperature(_),
                    MeasurementUnit::Celsius
                ) | (
                    "humidity",
                    FunctionKind::Humidity(_),
                    MeasurementUnit::Percentage
                ) | ("co2", FunctionKind::CarbonDioxide(_), MeasurementUnit::Ppm)
            ) {
                return Err(CapabilityError::InvalidDescription("sensor topology"));
            }
        }
        if measurements.is_empty() && battery.is_some() {
            return Err(CapabilityError::InvalidDescription("battery topology"));
        }
        if controls
            .iter()
            .any(|function| function.lighting().is_some())
        {
            if controls.len() != 1 || controls[0].id() != "main" || refresh_trigger.is_some() {
                return Err(CapabilityError::InvalidDescription("light topology"));
            }
        } else if !controls.is_empty() {
            let ids = controls
                .iter()
                .map(|function| function.id())
                .collect::<Vec<_>>();
            let single_main = ids == ["main"] && refresh_trigger.is_none();
            let cp7pd = ids == ["outlet_1", "outlet_2", "usb"] && refresh_trigger.is_some();
            if !(single_main || cp7pd) {
                return Err(CapabilityError::InvalidDescription("outlet topology"));
            }
        } else if refresh_trigger.is_some() {
            return Err(CapabilityError::InvalidDescription("sensor topology"));
        }
        Ok(Self {
            key,
            functions,
            refresh_trigger,
            battery,
            battery_issue,
        })
    }
    pub fn key(&self) -> &DeviceKey {
        &self.key
    }
    pub fn functions(&self) -> &[ParsedFunction] {
        &self.functions
    }
    pub fn refresh_trigger(&self) -> Option<PropertyBinding> {
        self.refresh_trigger
    }
    pub fn battery(&self) -> Option<&BatteryBinding> {
        self.battery.as_ref()
    }
    pub fn battery_issue(&self) -> Option<&str> {
        self.battery_issue.as_deref()
    }
    pub fn is_read_only(&self) -> bool {
        !self.functions.is_empty()
            && self
                .functions
                .iter()
                .all(|function| function.measurement().is_some())
    }
    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        let description = PhysicalDeviceDescription::new(
            self.key.clone(),
            label,
            self.functions
                .iter()
                .map(ParsedFunction::description)
                .collect(),
        )
        .expect("a parsed device keeps a valid description");
        if self.battery.is_some() {
            description.with_battery()
        } else {
            description
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    InvalidDescription(&'static str),
    UnsupportedDevice,
    UnsupportedLightGroup,
    MissingMainPower,
    InvalidMainPowerType,
    MissingAccess(&'static str),
    AmbiguousMainPower,
    MissingFunction(&'static str),
    MissingMeasurement(&'static str),
    InvalidMeasurement(&'static str),
    IncompatibleModel(&'static str),
    InvalidLight(&'static str),
    InvalidGroup(&'static str),
    InvalidValue(&'static str),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDescription(field) => {
                write!(formatter, "invalid MIoT description: {field}")
            }
            Self::UnsupportedDevice => formatter.write_str("unsupported MIoT device type"),
            Self::UnsupportedLightGroup => {
                formatter.write_str("Xiaomi light groups are not supported")
            }
            Self::MissingMainPower => formatter.write_str("standard outlet main power is missing"),
            Self::InvalidMainPowerType => formatter.write_str("outlet switch must be boolean"),
            Self::MissingAccess(access) => {
                write!(formatter, "required property lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
            Self::MissingFunction(id) => {
                write!(formatter, "required binding is missing: {id}")
            }
            Self::MissingMeasurement(id) => {
                write!(formatter, "required measurement is missing: {id}")
            }
            Self::InvalidMeasurement(field) => {
                write!(formatter, "invalid measurement capability: {field}")
            }
            Self::InvalidLight(field) => write!(formatter, "invalid light capability: {field}"),
            Self::InvalidGroup(field) => write!(formatter, "invalid native light group: {field}"),
            Self::InvalidValue(field) => write!(formatter, "invalid light property value: {field}"),
            Self::IncompatibleModel(model) => {
                write!(formatter, "unsupported MIoT description for {model}")
            }
        }
    }
}

impl Error for CapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericFormat {
    Uint8,
    Uint16,
    Uint32,
    Int8,
    Int16,
    Int32,
    Float,
}
impl NumericFormat {
    pub(super) fn maximum(self) -> u32 {
        match self {
            Self::Uint8 => u8::MAX.into(),
            Self::Uint16 => u16::MAX.into(),
            Self::Uint32 => u32::MAX,
            Self::Int8 => i8::MAX as u32,
            Self::Int16 => i16::MAX as u32,
            Self::Int32 => i32::MAX as u32,
            Self::Float => u32::MAX,
        }
    }
    fn measurement_bounds(self) -> Option<(f64, f64)> {
        Some(match self {
            Self::Uint8 => (0.0, u8::MAX as f64),
            Self::Uint16 => (0.0, u16::MAX as f64),
            Self::Uint32 => (0.0, u32::MAX as f64),
            Self::Int8 => (i8::MIN as f64, i8::MAX as f64),
            Self::Int16 => (i16::MIN as f64, i16::MAX as f64),
            Self::Int32 => (i32::MIN as f64, i32::MAX as f64),
            Self::Float => return None,
        })
    }
}

fn raw_integer(value: &Value) -> Result<u32, CapabilityError> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(CapabilityError::InvalidValue("type"))
}
fn grid_raw(value: &Value, range: NumericRange) -> Result<u32, CapabilityError> {
    let raw = raw_integer(value)?;
    if !range.contains(raw) {
        return Err(CapabilityError::InvalidValue("range or step"));
    }
    Ok(raw)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementUnit {
    Celsius,
    Percentage,
    Ppm,
}

/// One real read-only measurement binding. Format, unit, range and access are
/// fixed at the source boundary; Matter projects its own target representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MeasurementBinding {
    property: PropertyBinding,
    format: NumericFormat,
    unit: MeasurementUnit,
    range: MeasurementRange,
}
impl MeasurementBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        unit: MeasurementUnit,
        range: MeasurementRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0 || property.piid == 0 {
            return Err(CapabilityError::InvalidMeasurement("range"));
        }
        if let Some((min, max)) = format.measurement_bounds()
            && (range.min() < min || range.max() > max)
        {
            return Err(CapabilityError::InvalidMeasurement("range"));
        }
        Ok(Self {
            property,
            format,
            unit,
            range,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn unit(&self) -> MeasurementUnit {
        self.unit
    }
    pub fn range(&self) -> MeasurementRange {
        self.range
    }
    /// Decode a real source value in its declared unit. Integer formats do not
    /// accept fractional JSON values; every format is finite and in range.
    pub fn read_value(&self, value: &Value) -> Result<f64, CapabilityError> {
        let value = match self.format {
            NumericFormat::Float => value.as_f64(),
            _ => value
                .as_i64()
                .map(|value| value as f64)
                .or_else(|| value.as_u64().map(|value| value as f64))
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|value| value.is_finite() && value.fract() == 0.0)
                }),
        }
        .filter(|value| self.range.contains(*value))
        .ok_or(CapabilityError::InvalidValue("measurement"))?;
        Ok(value)
    }
}

/// One optional physical battery binding. The service/property semantics are
/// resolved before construction; this type only carries the real binding and
/// its supported percentage representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryBinding {
    measurement: MeasurementBinding,
}
impl BatteryBinding {
    pub(crate) fn new(measurement: MeasurementBinding) -> Result<Self, CapabilityError> {
        if measurement.unit() != MeasurementUnit::Percentage
            || measurement.range().min() < 0.0
            || measurement.range().max() > 100.0
        {
            return Err(CapabilityError::InvalidMeasurement("battery"));
        }
        Ok(Self { measurement })
    }
    pub fn property(&self) -> PropertyBinding {
        self.measurement.property()
    }
    pub fn format(&self) -> NumericFormat {
        self.measurement.format()
    }
    pub fn range(&self) -> MeasurementRange {
        self.measurement.range()
    }
    pub fn read_value(&self, value: &Value) -> Result<f64, CapabilityError> {
        self.measurement
            .read_value(value)
            .map_err(|_| CapabilityError::InvalidValue("battery"))
    }
}

/// A native brightness grid. Construction guarantees a nonzero reachable
/// span that the declared format can hold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrightnessBinding {
    property: PropertyBinding,
    format: NumericFormat,
    range: NumericRange,
}
impl BrightnessBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        range: NumericRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || range.nonzero_bounds().is_none()
            || range.max() > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("brightness range or format"));
        }
        Ok(Self {
            property,
            format,
            range,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn range(&self) -> NumericRange {
        self.range
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        grid_raw(value, self.range)
    }
    pub fn brightness_from_raw(&self, value: &Value) -> Result<f64, CapabilityError> {
        let raw = self.read_raw(value)?;
        if raw == 0 {
            return Ok(0.0);
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .expect("a valid brightness grid has a nonzero span");
        Ok(1.0 + (raw - min) as f64 * 99.0 / (max - min) as f64)
    }
    /// Convert an already validated brightness; zero uses a reachable zero.
    /// Quantization to the native grid stays at the Xiaomi source boundary.
    pub fn brightness_to_raw(&self, percent: f64) -> u32 {
        if percent == 0.0 && self.range.min() == 0 {
            return 0;
        }
        let (min, max) = self
            .range
            .nonzero_bounds()
            .expect("a valid brightness grid has a nonzero span");
        self.range
            .quantize(min as f64 + (percent - 1.0) * (max - min) as f64 / 99.0)
            .max(min)
    }
}

/// A native color-temperature grid plus the logical interval it advertises.
/// The grid keeps its own step for Xiaomi quantization; the interval carries
/// no step and is what publication compares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemperatureBinding {
    property: PropertyBinding,
    format: NumericFormat,
    range: NumericRange,
    interval: TemperatureInterval,
}
impl TemperatureBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        range: NumericRange,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || range.min() == 0
            || range.max() > 1_000_000
            || range.max() > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("temperature range or format"));
        }
        let interval = TemperatureInterval::from_grid(range)
            .map_err(|_| CapabilityError::InvalidLight("temperature range or format"))?;
        Ok(Self {
            property,
            format,
            range,
            interval,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn range(&self) -> NumericRange {
        self.range
    }
    pub fn interval(&self) -> TemperatureInterval {
        self.interval
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        grid_raw(value, self.range)
    }
    /// Quantize a kelvin target onto the native grid. The target keeps full
    /// precision until this single source-boundary quantization.
    pub fn temperature_to_raw(&self, kelvin: f64) -> u32 {
        self.range.quantize(kelvin)
    }
}

/// A native RGB binding. The advertised range is fixed at `0..=0xffffff`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorBinding {
    property: PropertyBinding,
    format: NumericFormat,
}
impl ColorBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || format.maximum() < 0xffffff
        {
            return Err(CapabilityError::InvalidLight("rgb range or format"));
        }
        Ok(Self { property, format })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn read_raw(&self, value: &Value) -> Result<u32, CapabilityError> {
        let raw = raw_integer(value)?;
        if raw > 0xffffff {
            return Err(CapabilityError::InvalidValue("range or step"));
        }
        Ok(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeBinding {
    property: PropertyBinding,
    format: NumericFormat,
    temperature_value: u32,
    rgb_value: u32,
}
impl ModeBinding {
    pub(crate) fn new(
        property: PropertyBinding,
        format: NumericFormat,
        temperature_value: u32,
        rgb_value: u32,
    ) -> Result<Self, CapabilityError> {
        if property.siid == 0
            || property.piid == 0
            || format == NumericFormat::Float
            || temperature_value == rgb_value
            || temperature_value > format.maximum()
            || rgb_value > format.maximum()
        {
            return Err(CapabilityError::InvalidLight("mode"));
        }
        Ok(Self {
            property,
            format,
            temperature_value,
            rgb_value,
        })
    }
    pub fn property(&self) -> PropertyBinding {
        self.property
    }
    pub fn format(&self) -> NumericFormat {
        self.format
    }
    pub fn temperature_value(&self) -> u32 {
        self.temperature_value
    }
    pub fn rgb_value(&self) -> u32 {
        self.rgb_value
    }
    pub fn read_mode(&self, value: &Value) -> Result<LightMode, CapabilityError> {
        match value.as_u64() {
            Some(value) if value == self.temperature_value as u64 => Ok(LightMode::Temperature),
            Some(value) if value == self.rgb_value as u64 => Ok(LightMode::Rgb),
            _ => Err(CapabilityError::InvalidValue("mode")),
        }
    }
}

/// Real light bindings in one of the four supported profiles. The stored
/// representation is the closed valid value; construction from the optional
/// real bindings is the only fallible step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightBindings {
    OnOff,
    Dimmable {
        brightness: BrightnessBinding,
    },
    ColorTemperature {
        brightness: BrightnessBinding,
        temperature: TemperatureBinding,
    },
    ExtendedColor {
        brightness: BrightnessBinding,
        temperature: TemperatureBinding,
        color: ColorBinding,
        mode: Option<ModeBinding>,
    },
}
impl LightBindings {
    pub(crate) fn new(
        brightness: Option<BrightnessBinding>,
        temperature: Option<TemperatureBinding>,
        color: Option<ColorBinding>,
        mode: Option<ModeBinding>,
    ) -> Result<Self, CapabilityError> {
        match (brightness, temperature, color, mode) {
            (None, None, None, None) => Ok(Self::OnOff),
            (Some(brightness), None, None, None) => Ok(Self::Dimmable { brightness }),
            (Some(brightness), Some(temperature), None, None) => Ok(Self::ColorTemperature {
                brightness,
                temperature,
            }),
            (Some(brightness), Some(temperature), Some(color), mode) => Ok(Self::ExtendedColor {
                brightness,
                temperature,
                color,
                mode,
            }),
            _ => Err(CapabilityError::InvalidLight(
                "unsupported light capability combination",
            )),
        }
    }
    pub fn brightness(&self) -> Option<&BrightnessBinding> {
        match self {
            Self::OnOff => None,
            Self::Dimmable { brightness }
            | Self::ColorTemperature { brightness, .. }
            | Self::ExtendedColor { brightness, .. } => Some(brightness),
        }
    }
    pub fn temperature(&self) -> Option<&TemperatureBinding> {
        match self {
            Self::OnOff | Self::Dimmable { .. } => None,
            Self::ColorTemperature { temperature, .. }
            | Self::ExtendedColor { temperature, .. } => Some(temperature),
        }
    }
    pub fn color(&self) -> Option<&ColorBinding> {
        match self {
            Self::ExtendedColor { color, .. } => Some(color),
            Self::OnOff | Self::Dimmable { .. } | Self::ColorTemperature { .. } => None,
        }
    }
    pub fn mode(&self) -> Option<&ModeBinding> {
        match self {
            Self::ExtendedColor { mode, .. } => mode.as_ref(),
            Self::OnOff | Self::Dimmable { .. } | Self::ColorTemperature { .. } => None,
        }
    }
    pub fn profile(&self) -> LightProfile {
        match self {
            Self::OnOff => LightProfile::OnOff,
            Self::Dimmable { .. } => LightProfile::Dimmable,
            Self::ColorTemperature { temperature, .. } => {
                LightProfile::ColorTemperature(temperature.interval())
            }
            Self::ExtendedColor { temperature, .. } => {
                LightProfile::ExtendedColor(temperature.interval())
            }
        }
    }
    pub(crate) fn bindings(&self) -> Vec<PropertyBinding> {
        let mut bindings = Vec::new();
        if let Some(brightness) = self.brightness() {
            bindings.push(brightness.property());
        }
        if let Some(temperature) = self.temperature() {
            bindings.push(temperature.property());
        }
        if let Some(color) = self.color() {
            bindings.push(color.property());
        }
        if let Some(mode) = self.mode() {
            bindings.push(mode.property());
        }
        bindings
    }
}
