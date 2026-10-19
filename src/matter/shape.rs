//! Canonical fixed Matter shape for one published device.
//!
//! The frozen model and publication compare this exact surface when deciding
//! root/endpoint ConfigurationVersion changes: the four standard light
//! profiles with their published CT mired bounds, and outlet function order
//! (which fixes parent/child descriptor tags). Endpoint numbers, bindings,
//! live state and every generated attribute/command are deliberately excluded.
//! Equivalent Kelvin encodings of the same mired span compare equal.

use crate::device::{
    FunctionDescription, FunctionKind, LightProfileKind, PhysicalDeviceDescription,
};
use crate::lighting::temperature;

#[derive(Clone, Debug, Eq, PartialEq)]
struct FixedShape {
    functions: Vec<FixedFunction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FixedFunction {
    id: String,
    kind: FixedKind,
    /// Only multi-function outlets publish child labels in the Descriptor
    /// TagList; a single function's label is not part of the fixed surface.
    child_label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FixedKind {
    Outlet,
    Light {
        profile: LightProfileKind,
        mired: Option<(u16, u16)>,
    },
}

/// Canonical shape of the functions as they will be published.
fn fixed_shape(functions: &[FunctionDescription]) -> FixedShape {
    let multiple = functions.len() > 1;
    FixedShape {
        functions: functions
            .iter()
            .map(|function| FixedFunction {
                id: function.id.clone(),
                kind: match function.kind {
                    FunctionKind::Outlet => FixedKind::Outlet,
                    FunctionKind::Light(profile) => FixedKind::Light {
                        profile: profile.kind(),
                        mired: profile
                            .temperature_interval()
                            .map(temperature::mired_bounds),
                    },
                },
                child_label: multiple.then(|| function.label.clone()),
            })
            .collect(),
    }
}

/// True when two descriptions publish the same fixed Matter surface.
/// NodeLabel and endpoint numbers are compared separately by publication.
pub(crate) fn same_fixed_description(
    first: &PhysicalDeviceDescription,
    second: &PhysicalDeviceDescription,
) -> bool {
    fixed_shape(first.functions()) == fixed_shape(second.functions())
}

#[cfg(test)]
#[path = "tests/shape.rs"]
mod tests;
