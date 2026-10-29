//! Canonical fixed Matter shape for one published device.
//!
//! The frozen model and publication compare this exact surface when deciding
//! root/endpoint ConfigurationVersion changes: the four standard light
//! profiles with their published CT mired bounds, projected measurement ranges,
//! visible function order and child tags, and the physical battery capability
//! that decides the parent Power Source topology. Endpoint numbers, bindings,
//! live state and every generated attribute/command are deliberately excluded.
//! Equivalent Kelvin encodings of the same mired span compare equal.

use super::units;
use crate::device::{FunctionDescription, FunctionKind, LightProfileKind};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FixedShape {
    functions: Vec<FixedFunction>,
    battery: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FixedFunction {
    id: String,
    kind: FixedKind,
    /// Compound controls and all sensor layouts publish child labels in
    /// Descriptor TagList; a single control keeps its combined endpoint.
    child_label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FixedKind {
    Outlet,
    Curtain,
    Fan {
        horizontal_swing: bool,
        speed_count: u8,
    },
    Contact,
    Motion,
    Occupancy,
    Light {
        profile: LightProfileKind,
        mired: Option<(u16, u16)>,
    },
    Temperature {
        bounds: Option<(i16, i16)>,
    },
    Humidity {
        bounds: Option<(u16, u16)>,
    },
    CarbonDioxide {
        bounds: Option<(u32, u32)>,
    },
    Illuminance {
        bounds: Option<(u16, u16)>,
    },
}

/// Project the actual visible functions into the canonical publication shape.
/// Planning compares this value for the committed and final candidate graphs.
pub(crate) fn fixed_shape_with_battery<'a>(
    functions: impl ExactSizeIterator<Item = &'a FunctionDescription> + Clone,
    battery: bool,
) -> FixedShape {
    let multiple = functions.len() > 1 || functions.clone().any(|function| function.is_read_only());
    FixedShape {
        battery,
        functions: functions
            .map(|function| FixedFunction {
                id: function.id.clone(),
                kind: match function.kind {
                    FunctionKind::Outlet => FixedKind::Outlet,
                    FunctionKind::Curtain => FixedKind::Curtain,
                    FunctionKind::Fan {
                        horizontal_swing,
                        speed_count,
                    } => FixedKind::Fan {
                        horizontal_swing,
                        speed_count,
                    },
                    FunctionKind::Contact => FixedKind::Contact,
                    FunctionKind::Motion => FixedKind::Motion,
                    FunctionKind::Occupancy => FixedKind::Occupancy,
                    FunctionKind::Light(profile) => FixedKind::Light {
                        profile: profile.kind(),
                        mired: profile.temperature_interval().map(units::mired_bounds),
                    },
                    FunctionKind::Temperature(range) => FixedKind::Temperature {
                        bounds: units::temperature_matter_bounds(range),
                    },
                    FunctionKind::Humidity(range) => FixedKind::Humidity {
                        bounds: units::humidity_matter_bounds(range),
                    },
                    FunctionKind::Illuminance(range) => FixedKind::Illuminance {
                        bounds: units::illuminance_matter_bounds(range),
                    },
                    FunctionKind::CarbonDioxide(range) => FixedKind::CarbonDioxide {
                        bounds: units::co2_matter_bounds(range)
                            .map(|(min, max)| (min.to_bits(), max.to_bits())),
                    },
                },
                child_label: multiple.then(|| function.label.clone()),
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "tests/shape.rs"]
mod tests;
