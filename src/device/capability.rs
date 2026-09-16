use super::InvalidValue;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    Light,
    Switch,
    Plug,
    AirConditioner,
    Curtain,
    Fan,
    Sensor,
    Vacuum,
    BathHeater,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumericRange {
    pub minimum: f64,
    pub maximum: f64,
    pub step: f64,
    pub unit: NumericUnit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumericUnit {
    Percent,
    Celsius,
    Kelvin,
    Lux,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HvacMode {
    Off,
    Auto,
    Cool,
    Heat,
    Dry,
    FanOnly,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwingMode {
    Off,
    Vertical,
    Horizontal,
    Both,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VacuumCleanMode {
    Vacuum,
    Mop,
    VacuumAndMop,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VacuumOperationalState {
    Idle,
    Cleaning,
    Paused,
    Returning,
    Charging,
    Docked,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SensingModality {
    /// The sensor reports presence through a modality not classified by the catalog.
    Unspecified,
    Pir,
    Radar,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Capability {
    Power { writable: bool },
    Brightness(NumericRange),
    ColorTemperature(NumericRange),
    Color,
    TargetTemperature(NumericRange),
    HvacModes(Vec<HvacMode>),
    FanSpeeds(Vec<u16>),
    SwingModes(Vec<SwingMode>),
    CurtainPosition(NumericRange),
    CurtainStop,
    Temperature(NumericRange),
    Humidity(NumericRange),
    Illuminance(NumericRange),
    Motion,
    Occupancy,
    SensingModalities(Vec<SensingModality>),
    Contact,
    Battery(NumericRange),
    VacuumCleanModes(Vec<VacuumCleanMode>),
    VacuumControl,
    VacuumDock,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeatureCapabilities(pub Vec<Capability>);
impl FeatureCapabilities {
    pub fn sensing_modalities(&self) -> &[SensingModality] {
        self.0
            .iter()
            .find_map(|capability| match capability {
                Capability::SensingModalities(modalities) => Some(modalities.as_slice()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn light(dimmable: bool, color_temperature: bool) -> Self {
        let mut v = vec![Capability::Power { writable: true }];
        if dimmable {
            v.push(Capability::Brightness(NumericRange {
                minimum: 0.,
                maximum: 100.,
                step: 1.,
                unit: NumericUnit::Percent,
            }));
        }
        if color_temperature {
            v.push(Capability::ColorTemperature(NumericRange {
                minimum: 2000.,
                maximum: 6500.,
                step: 1.,
                unit: NumericUnit::Kelvin,
            }));
        }
        Self(v)
    }

    pub fn validate(&self, command: &DeviceCommand) -> Result<(), CommandValidationError> {
        let supported = match command {
            DeviceCommand::SetPower(_) => self
                .0
                .iter()
                .any(|capability| matches!(capability, Capability::Power { writable: true })),
            DeviceCommand::SetBrightness(value) => self
                .numeric(CapabilityKind::Brightness)
                .is_some_and(|range| range.accepts(value.get())),
            DeviceCommand::SetColorTemperature(value) => self
                .numeric(CapabilityKind::ColorTemperature)
                .is_some_and(|range| range.accepts(*value as f64)),
            DeviceCommand::SetColor(_) => self.0.contains(&Capability::Color),
            DeviceCommand::SetTargetTemperature(value) => self
                .numeric(CapabilityKind::TargetTemperature)
                .is_some_and(|range| range.accepts(*value)),
            DeviceCommand::SetHvacMode(value) => self.0.iter().any(
                |capability| matches!(capability, Capability::HvacModes(values) if values.contains(value)),
            ),
            DeviceCommand::SetFanSpeed(value) => self.0.iter().any(
                |capability| matches!(capability, Capability::FanSpeeds(values) if values.contains(value)),
            ),
            DeviceCommand::SetSwingMode(value) => self.0.iter().any(
                |capability| matches!(capability, Capability::SwingModes(values) if values.contains(value)),
            ),
            DeviceCommand::SetCurtainPosition(value) => self
                .numeric(CapabilityKind::CurtainPosition)
                .is_some_and(|range| range.accepts(value.get())),
            DeviceCommand::StopCurtain => self.0.contains(&Capability::CurtainStop),
            DeviceCommand::SetOscillation(enabled) => self.0.iter().any(|capability| {
                matches!(capability, Capability::SwingModes(values) if if *enabled {
                    values.iter().any(|mode| *mode != SwingMode::Off)
                } else {
                    values.contains(&SwingMode::Off)
                })
            }),
            DeviceCommand::StartVacuum | DeviceCommand::StopVacuum => {
                self.0.contains(&Capability::VacuumControl)
            }
            DeviceCommand::ReturnVacuumToDock => self.0.contains(&Capability::VacuumDock),
            DeviceCommand::SetVacuumCleanMode(value) => self.0.iter().any(
                |capability| matches!(capability, Capability::VacuumCleanModes(values) if values.contains(value)),
            ),
        };
        supported
            .then_some(())
            .ok_or(CommandValidationError::UnsupportedValue)
    }

    fn numeric(&self, kind: CapabilityKind) -> Option<&NumericRange> {
        self.0
            .iter()
            .find_map(|capability| match (kind, capability) {
                (CapabilityKind::Brightness, Capability::Brightness(range))
                | (CapabilityKind::ColorTemperature, Capability::ColorTemperature(range))
                | (CapabilityKind::TargetTemperature, Capability::TargetTemperature(range))
                | (CapabilityKind::CurtainPosition, Capability::CurtainPosition(range)) => {
                    Some(range)
                }
                _ => None,
            })
    }
}

#[derive(Clone, Copy)]
enum CapabilityKind {
    Brightness,
    ColorTemperature,
    TargetTemperature,
    CurtainPosition,
}
impl NumericRange {
    pub fn accepts(self, value: f64) -> bool {
        if !value.is_finite() || value < self.minimum || value > self.maximum || self.step <= 0.0 {
            return false;
        }
        let steps = (value - self.minimum) / self.step;
        (steps - steps.round()).abs() < 1e-6
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandValidationError {
    UnsupportedValue,
}
impl std::fmt::Display for CommandValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("command is not supported by this feature")
    }
}
impl std::error::Error for CommandValidationError {}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Percent(f64);
impl Percent {
    pub fn new(value: impl Into<f64>) -> Result<Self, InvalidValue> {
        let value = value.into();
        (value.is_finite() && (0.0..=100.0).contains(&value))
            .then_some(Self(value))
            .ok_or(InvalidValue)
    }
    pub fn get(self) -> f64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceCommand {
    SetPower(bool),
    SetBrightness(Percent),
    SetColorTemperature(u32),
    SetColor(RgbColor),
    SetTargetTemperature(f64),
    SetHvacMode(HvacMode),
    SetFanSpeed(u16),
    SetSwingMode(SwingMode),
    SetCurtainPosition(Percent),
    StopCurtain,
    SetOscillation(bool),
    StartVacuum,
    StopVacuum,
    ReturnVacuumToDock,
    SetVacuumCleanMode(VacuumCleanMode),
}
