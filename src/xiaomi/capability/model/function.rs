use super::super::{MotionBinding, StateBinding};
use super::{
    AirConditionerBindings, CurtainBindings, FanBindings, LightBindings, MeasurementBinding,
    PropertyBinding,
};
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
    AirConditioner {
        air_conditioner: AirConditionerBindings,
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
    pub(crate) fn air_conditioner_control(
        label: impl Into<String>,
        air_conditioner: AirConditionerBindings,
    ) -> Self {
        Self {
            id: air_conditioner.function_id(),
            label: label.into(),
            bindings: FunctionBindings::AirConditioner { air_conditioner },
        }
    }
    pub(crate) fn air_conditioner(&self) -> Option<&AirConditionerBindings> {
        match &self.bindings {
            FunctionBindings::AirConditioner { air_conditioner } => Some(air_conditioner),
            _ => None,
        }
    }
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
    /// Stable source identity from the owning service or observed property.
    pub(crate) fn source_id(&self) -> String {
        match &self.bindings {
            FunctionBindings::Outlet { power } => format!("outlet:{}", power.siid),
            FunctionBindings::Light { power, .. } => format!("light:{}", power.siid),
            FunctionBindings::Fan { fan } => format!("fan:{}", fan.power().siid),
            FunctionBindings::AirConditioner { air_conditioner } => air_conditioner.function_id(),
            FunctionBindings::Curtain { curtain } => {
                format!("curtain:{}", curtain.motor().property().siid)
            }
            FunctionBindings::Temperature { measurement } => {
                property_id("temperature", measurement.property())
            }
            FunctionBindings::Humidity { measurement } => {
                property_id("humidity", measurement.property())
            }
            FunctionBindings::CarbonDioxide { measurement } => {
                property_id("co2", measurement.property())
            }
            FunctionBindings::Illuminance { measurement } => {
                property_id("illuminance", measurement.property())
            }
            FunctionBindings::Contact { state } => property_id("contact", state.property()),
            FunctionBindings::Motion { motion } => property_id("motion", motion.state().property()),
            FunctionBindings::Occupancy { state } => property_id("occupancy", state.property()),
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
            FunctionBindings::AirConditioner { air_conditioner } => Some(air_conditioner.power()),
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
            | FunctionBindings::AirConditioner { .. }
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
            | FunctionBindings::AirConditioner { .. }
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
                speed_count: fan.speed().count(),
            },
            FunctionBindings::AirConditioner { air_conditioner } => FunctionKind::AirConditioner {
                temperature: air_conditioner.temperature().grid(),
                vertical_swing: air_conditioner.swing().is_some(),
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
        if let Some(air_conditioner) = self.air_conditioner() {
            return air_conditioner.bindings();
        }
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

fn property_id(kind: &str, binding: PropertyBinding) -> String {
    format!("{kind}:{}:{}", binding.siid, binding.piid)
}
