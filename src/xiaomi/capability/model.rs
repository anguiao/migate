use std::{error::Error, fmt};

use crate::device::{
    DeviceKey, FunctionDescription, FunctionKind, LightMode, LightProfile, NumericRange,
    PhysicalDeviceDescription, TemperatureInterval,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
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
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn power(&self) -> PropertyBinding {
        match &self.bindings {
            FunctionBindings::Outlet { power } | FunctionBindings::Light { power, .. } => *power,
        }
    }
    pub fn lighting(&self) -> Option<&LightBindings> {
        match &self.bindings {
            FunctionBindings::Outlet { .. } => None,
            FunctionBindings::Light { lighting, .. } => Some(lighting),
        }
    }
    pub fn kind(&self) -> FunctionKind {
        match &self.bindings {
            FunctionBindings::Outlet { .. } => FunctionKind::Outlet,
            FunctionBindings::Light { lighting, .. } => FunctionKind::Light(lighting.profile()),
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
        let mut bindings = vec![self.power()];
        if let Some(lighting) = self.lighting() {
            bindings.extend(lighting.bindings());
        }
        bindings
    }
}

/// A parsed device description with non-empty, uniquely keyed functions and
/// uniquely owned real bindings. Lights use the single `main` function layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDevice {
    key: DeviceKey,
    functions: Vec<ParsedFunction>,
    refresh_trigger: Option<PropertyBinding>,
}
impl ParsedDevice {
    pub(crate) fn new(
        key: DeviceKey,
        functions: Vec<ParsedFunction>,
        refresh_trigger: Option<PropertyBinding>,
    ) -> Result<Self, CapabilityError> {
        if functions.is_empty() {
            return Err(CapabilityError::InvalidDescription("functions"));
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
        {
            if binding.siid == 0 || binding.piid == 0 || !bindings.insert(binding) {
                return Err(CapabilityError::InvalidDescription("function bindings"));
            }
        }
        let lights = functions
            .iter()
            .filter(|function| function.lighting().is_some())
            .count();
        if lights > 0 {
            if functions.len() != 1 || functions[0].id() != "main" || refresh_trigger.is_some() {
                return Err(CapabilityError::InvalidDescription("light topology"));
            }
        } else {
            // Outlets support only a single main channel or the concrete
            // cuco.plug.cp7pd three-channel layout with its master trigger.
            let ids = functions.iter().map(ParsedFunction::id).collect::<Vec<_>>();
            let single_main = ids == ["main"] && refresh_trigger.is_none();
            let cp7pd = ids == ["outlet_1", "outlet_2", "usb"] && refresh_trigger.is_some();
            if !(single_main || cp7pd) {
                return Err(CapabilityError::InvalidDescription("outlet topology"));
            }
        }
        Ok(Self {
            key,
            functions,
            refresh_trigger,
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
    pub(crate) fn physical_description(
        &self,
        label: impl Into<String>,
    ) -> PhysicalDeviceDescription {
        PhysicalDeviceDescription::new(
            self.key.clone(),
            label,
            self.functions
                .iter()
                .map(ParsedFunction::description)
                .collect(),
        )
        .expect("a parsed device keeps a valid description")
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
                write!(formatter, "outlet switch lacks {access} access")
            }
            Self::AmbiguousMainPower => {
                formatter.write_str("multiple standard outlet main power properties")
            }
            Self::MissingFunction(id) => {
                write!(formatter, "required outlet binding is missing: {id}")
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
        }
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
        if property.siid == 0 || property.piid == 0 || format.maximum() < 0xffffff {
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
