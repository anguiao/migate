use std::{fmt, str::FromStr};

macro_rules! text_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidValue> {
                let value = value.into();
                validate_text(&value)?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = InvalidValue;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
text_id!(AccountId);
text_id!(HomeId);
text_id!(DeviceDid);
text_id!(FeatureId);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct PhysicalDeviceId {
    pub account: AccountId,
    pub home: HomeId,
    pub parent_did: DeviceDid,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub enum FeatureRole {
    Light,
    Load,
    Climate,
    Curtain,
    Fan,
    TemperatureSensor,
    HumiditySensor,
    IlluminanceSensor,
    MotionSensor,
    OccupancySensor,
    ContactSensor,
    Vacuum,
    BathHeaterLight,
    BathHeaterSupplyFan,
    BathHeaterExhaustFan,
    BathHeaterClimate,
}
impl FeatureRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Load => "load",
            Self::Climate => "climate",
            Self::Curtain => "curtain",
            Self::Fan => "fan",
            Self::TemperatureSensor => "temperature-sensor",
            Self::HumiditySensor => "humidity-sensor",
            Self::IlluminanceSensor => "illuminance-sensor",
            Self::MotionSensor => "motion-sensor",
            Self::OccupancySensor => "occupancy-sensor",
            Self::ContactSensor => "contact-sensor",
            Self::Vacuum => "vacuum",
            Self::BathHeaterLight => "bath-heater-light",
            Self::BathHeaterSupplyFan => "bath-heater-supply-fan",
            Self::BathHeaterExhaustFan => "bath-heater-exhaust-fan",
            Self::BathHeaterClimate => "bath-heater-climate",
        }
    }
}
impl FromStr for FeatureRole {
    type Err = InvalidValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "light" => Ok(Self::Light),
            "load" => Ok(Self::Load),
            "climate" => Ok(Self::Climate),
            "curtain" => Ok(Self::Curtain),
            "fan" => Ok(Self::Fan),
            "temperature-sensor" => Ok(Self::TemperatureSensor),
            "humidity-sensor" => Ok(Self::HumiditySensor),
            "illuminance-sensor" => Ok(Self::IlluminanceSensor),
            "motion-sensor" => Ok(Self::MotionSensor),
            "occupancy-sensor" => Ok(Self::OccupancySensor),
            "contact-sensor" => Ok(Self::ContactSensor),
            "vacuum" => Ok(Self::Vacuum),
            "bath-heater-light" => Ok(Self::BathHeaterLight),
            "bath-heater-supply-fan" => Ok(Self::BathHeaterSupplyFan),
            "bath-heater-exhaust-fan" => Ok(Self::BathHeaterExhaustFan),
            "bath-heater-climate" => Ok(Self::BathHeaterClimate),
            _ => Err(InvalidValue),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct FeatureIdentity {
    pub physical: PhysicalDeviceId,
    pub service_instance: u32,
    pub role: FeatureRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidValue;
impl fmt::Display for InvalidValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value is invalid")
    }
}
impl std::error::Error for InvalidValue {}
fn validate_text(value: &str) -> Result<(), InvalidValue> {
    if value.is_empty() || value.chars().any(char::is_control) {
        Err(InvalidValue)
    } else {
        Ok(())
    }
}
