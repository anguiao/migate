//! Canonical fixed Matter shape for one published device.
//!
//! The frozen model and publication compare this exact surface when deciding
//! root/endpoint ConfigurationVersion changes: the four standard light
//! profiles with their published CT mired bounds, and outlet function order
//! (which fixes parent/child descriptor tags). Endpoint numbers, bindings,
//! live state and every generated attribute/command are deliberately excluded.
//! Equivalent Kelvin encodings of the same mired span compare equal.

use crate::device::{Category, FunctionDescription, LightProfile, PhysicalDeviceDescription};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FixedShape {
    pub(crate) functions: Vec<FixedFunction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FixedFunction {
    pub(crate) id: String,
    pub(crate) category: Category,
    /// Only multi-function outlets publish child labels in the Descriptor
    /// TagList; a single function's label is not part of the fixed surface.
    pub(crate) child_label: Option<String>,
    pub(crate) light: Option<FixedLight>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FixedLight {
    pub(crate) profile: LightProfile,
    pub(crate) mired: Option<(u16, u16)>,
}

/// Canonical shape of the functions as they will be published.
pub(crate) fn fixed_shape(functions: &[FunctionDescription]) -> FixedShape {
    let multiple = functions.len() > 1;
    FixedShape {
        functions: functions
            .iter()
            .map(|function| FixedFunction {
                id: function.id.clone(),
                category: function.category,
                child_label: multiple.then(|| function.label.clone()),
                light: function.light_profile().ok().map(|profile| FixedLight {
                    profile,
                    mired: function
                        .temperature_range()
                        .and_then(|range| range.mired_bounds().ok()),
                }),
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
    fixed_shape(&first.functions) == fixed_shape(&second.functions)
}

#[cfg(test)]
#[path = "tests/shape.rs"]
mod tests;
