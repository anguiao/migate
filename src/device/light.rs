//! A generic light control entry: one standalone function or one virtual
//! group. This facade carries no Matter endpoint type, so protocol entries
//! read the same state shape and applied member functions for both.

use super::{Function, Group, GroupSnapshot};

/// A standalone physical function or a virtual group over its applied
/// members. Cloning keeps the same function or group instance.
#[derive(Clone)]
pub enum Light {
    Single(Function),
    Group(Group),
}

impl Light {
    pub fn state(&self) -> GroupSnapshot {
        match self {
            Self::Single(function) => {
                let snapshot = function.snapshot();
                let fresh = function.fresh_snapshot();
                GroupSnapshot {
                    power: snapshot.power,
                    lighting: snapshot.lighting,
                    reachable: fresh.reachable,
                    representative: Some(function.function_key().clone()),
                    members: vec![fresh],
                }
            }
            Self::Group(group) => group.snapshot(),
        }
    }

    /// Applied members at the current boundary, in stable member order.
    pub fn functions(&self) -> Vec<Function> {
        match self {
            Self::Single(function) => vec![function.clone()],
            Self::Group(group) => group.functions(),
        }
    }

    pub fn function(&self) -> Option<&Function> {
        match self {
            Self::Single(function) => Some(function),
            Self::Group(_) => None,
        }
    }

    pub fn group(&self) -> Option<&Group> {
        match self {
            Self::Group(group) => Some(group),
            Self::Single(_) => None,
        }
    }
}
