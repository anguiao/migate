use super::{
    FeatureIdentity, HvacMode, Percent, RgbColor, SwingMode, VacuumCleanMode,
    VacuumOperationalState,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub enum Property {
    Power,
    Brightness,
    ColorTemperature,
    Color,
    CurrentTemperature,
    TargetTemperature,
    HvacMode,
    FanSpeed,
    SwingMode,
    CurtainPosition,
    CurtainTargetPosition,
    CurtainMovement,
    Oscillation,
    Temperature,
    Humidity,
    Illuminance,
    Motion,
    Occupancy,
    Contact,
    Battery,
    VacuumCleanMode,
    VacuumOperationalState,
    VacuumFault,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CurtainMovement {
    Stopped,
    Opening,
    Closing,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Vacant,
    Occupied,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PropertyValue {
    Power(bool),
    Percent(Percent),
    ColorTemperature(u32),
    Color(RgbColor),
    Temperature(f64),
    HvacMode(HvacMode),
    FanSpeed(u16),
    SwingMode(SwingMode),
    CurtainMovement(CurtainMovement),
    Oscillation(bool),
    Illuminance(f64),
    Motion(bool),
    Occupancy(PresenceState),
    ContactOpen(bool),
    VacuumCleanMode(VacuumCleanMode),
    VacuumOperationalState(VacuumOperationalState),
    VacuumFault(String),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateSource {
    Gateway,
    Lan,
    Cloud,
    Cache,
}
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyState {
    Current {
        value: PropertyValue,
        source: StateSource,
        observed_at: i64,
        report_version: u64,
    },
    LastKnown {
        value: PropertyValue,
        source: StateSource,
        observed_at: i64,
        report_version: u64,
    },
    Unknown {
        last_known: Option<KnownValue>,
        report_version: u64,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct KnownValue {
    pub value: PropertyValue,
    pub source: StateSource,
    pub observed_at: i64,
    pub report_version: u64,
}
impl PropertyState {
    pub(super) fn report_version(&self) -> u64 {
        match self {
            Self::Current { report_version, .. }
            | Self::LastKnown { report_version, .. }
            | Self::Unknown { report_version, .. } => *report_version,
        }
    }

    pub fn last_known(&self) -> Option<KnownValue> {
        match self {
            Self::Current {
                value,
                source,
                observed_at,
                report_version,
            }
            | Self::LastKnown {
                value,
                source,
                observed_at,
                report_version,
            } => Some(KnownValue {
                value: value.clone(),
                source: *source,
                observed_at: *observed_at,
                report_version: *report_version,
            }),
            Self::Unknown { last_known, .. } => last_known.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateReport {
    pub feature: FeatureIdentity,
    pub report_version: u64,
    pub source: StateSource,
    pub observed_at: i64,
    pub values: BTreeMap<Property, PropertyValue>,
}
impl StateReport {
    pub fn new(
        feature: FeatureIdentity,
        report_version: u64,
        source: StateSource,
        observed_at: i64,
        values: impl IntoIterator<Item = (Property, PropertyValue)>,
    ) -> Self {
        Self {
            feature,
            report_version,
            source,
            observed_at,
            values: values.into_iter().collect(),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateSnapshot {
    pub(super) properties: BTreeMap<Property, PropertyState>,
}
impl StateSnapshot {
    pub fn property(&self, p: Property) -> Option<&PropertyState> {
        self.properties.get(&p)
    }
    pub fn properties(&self) -> &BTreeMap<Property, PropertyState> {
        &self.properties
    }
}
