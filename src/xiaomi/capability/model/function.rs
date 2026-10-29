use super::super::{MotionBinding, StateBinding};
use super::{CurtainBindings, FanBindings, LightBindings, MeasurementBinding, PropertyBinding};
use crate::device::{FunctionDescription, FunctionKind};

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
    Curtain {
        curtain: CurtainBindings,
    },
    Fan {
        fan: FanBindings,
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
    Illuminance {
        measurement: MeasurementBinding,
    },
    Contact {
        state: StateBinding,
    },
    Motion {
        motion: MotionBinding,
    },
    Occupancy {
        state: StateBinding,
    },
}
impl ParsedFunction {
    pub(crate) fn fan_control(
        id: impl Into<String>,
        label: impl Into<String>,
        fan: FanBindings,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Fan { fan },
        }
    }
    pub fn fan(&self) -> Option<&FanBindings> {
        match &self.bindings {
            FunctionBindings::Fan { fan } => Some(fan),
            _ => None,
        }
    }

    pub(crate) fn curtain_control(
        id: impl Into<String>,
        label: impl Into<String>,
        curtain: CurtainBindings,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Curtain { curtain },
        }
    }
    pub fn curtain(&self) -> Option<&CurtainBindings> {
        match &self.bindings {
            FunctionBindings::Curtain { curtain } => Some(curtain),
            _ => None,
        }
    }
    pub(crate) fn contact(
        id: impl Into<String>,
        label: impl Into<String>,
        state: StateBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Contact { state },
        }
    }
    pub(crate) fn motion_sensor(
        id: impl Into<String>,
        label: impl Into<String>,
        motion: MotionBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Motion { motion },
        }
    }
    pub(crate) fn occupancy(
        id: impl Into<String>,
        label: impl Into<String>,
        state: StateBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Occupancy { state },
        }
    }
    pub fn state(&self) -> Option<&StateBinding> {
        match &self.bindings {
            FunctionBindings::Contact { state } | FunctionBindings::Occupancy { state } => {
                Some(state)
            }
            FunctionBindings::Motion { motion } => Some(motion.state()),
            _ => None,
        }
    }
    pub fn motion(&self) -> Option<&MotionBinding> {
        match &self.bindings {
            FunctionBindings::Motion { motion } => Some(motion),
            _ => None,
        }
    }
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
    pub(crate) fn illuminance(
        id: impl Into<String>,
        label: impl Into<String>,
        measurement: MeasurementBinding,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bindings: FunctionBindings::Illuminance { measurement },
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
            FunctionBindings::Fan { fan } => Some(fan.power()),
            FunctionBindings::Temperature { .. }
            | FunctionBindings::Curtain { .. }
            | FunctionBindings::Humidity { .. }
            | FunctionBindings::CarbonDioxide { .. }
            | FunctionBindings::Illuminance { .. }
            | FunctionBindings::Contact { .. }
            | FunctionBindings::Motion { .. }
            | FunctionBindings::Occupancy { .. } => None,
        }
    }
    pub fn lighting(&self) -> Option<&LightBindings> {
        match &self.bindings {
            FunctionBindings::Outlet { .. }
            | FunctionBindings::Fan { .. }
            | FunctionBindings::Curtain { .. }
            | FunctionBindings::Temperature { .. }
            | FunctionBindings::Humidity { .. }
            | FunctionBindings::CarbonDioxide { .. }
            | FunctionBindings::Illuminance { .. }
            | FunctionBindings::Contact { .. }
            | FunctionBindings::Motion { .. }
            | FunctionBindings::Occupancy { .. } => None,
            FunctionBindings::Light { lighting, .. } => Some(lighting),
        }
    }
    pub fn measurement(&self) -> Option<&MeasurementBinding> {
        match &self.bindings {
            FunctionBindings::Temperature { measurement }
            | FunctionBindings::Humidity { measurement }
            | FunctionBindings::CarbonDioxide { measurement }
            | FunctionBindings::Illuminance { measurement } => Some(measurement),
            FunctionBindings::Outlet { .. }
            | FunctionBindings::Light { .. }
            | FunctionBindings::Fan { .. }
            | FunctionBindings::Curtain { .. }
            | FunctionBindings::Contact { .. }
            | FunctionBindings::Motion { .. }
            | FunctionBindings::Occupancy { .. } => None,
        }
    }
    pub fn kind(&self) -> FunctionKind {
        match &self.bindings {
            FunctionBindings::Contact { .. } => FunctionKind::Contact,
            FunctionBindings::Motion { .. } => FunctionKind::Motion,
            FunctionBindings::Occupancy { .. } => FunctionKind::Occupancy,
            FunctionBindings::Outlet { .. } => FunctionKind::Outlet,
            FunctionBindings::Curtain { .. } => FunctionKind::Curtain,
            FunctionBindings::Fan { fan } => FunctionKind::Fan {
                horizontal_swing: fan.swing().is_some(),
            },
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
            FunctionBindings::Illuminance { measurement } => {
                FunctionKind::Illuminance(measurement.range())
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
        if let Some(fan) = self.fan() {
            return fan.bindings();
        }
        if let Some(curtain) = self.curtain() {
            return curtain.bindings();
        }
        if let Some(measurement) = self.measurement() {
            return vec![measurement.property()];
        }
        if let Some(state) = self.state() {
            return vec![state.property()];
        }
        let mut bindings = vec![self.power().expect("control functions have power")];
        if let Some(lighting) = self.lighting() {
            bindings.extend(lighting.bindings());
        }
        bindings
    }

    pub(crate) fn observation_bindings(&self) -> Vec<PropertyBinding> {
        if let Some(curtain) = self.curtain() {
            vec![
                curtain.status().property(),
                curtain.current().property(),
                curtain.target().property(),
            ]
        } else {
            self.bindings()
        }
    }
}
