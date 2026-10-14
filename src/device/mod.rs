mod command;
mod group;
mod light;
mod light_command;
mod model;
mod registry;
mod state;

pub use command::{CommandReceiver, CommandRequest};
pub use group::{Group, GroupDescription, GroupError, GroupSnapshot};
pub use light::Light;
pub use light_command::{
    ActionToken, DeviceRequest, LightAction, LightActionChanges, LightActionDiagnostics,
    LightActionResult, LightColorTarget, LightDimension, LightRequest, LightTarget,
};
pub use model::{
    Availability, Capability, Category, Command, CommandError, Composition, DeviceKey,
    FreshSnapshot, FunctionDescription, FunctionKey, FunctionSnapshot, LightMode, LightPatch,
    LightSnapshot, NumericRange, PhysicalDeviceDescription, Power, Rgb,
    brightness_from_matter_level, brightness_to_matter_level, derive_group_capabilities,
};
pub(crate) use registry::validate_composition_structure;
pub use registry::{
    CompositionError, CompositionSnapshot, DeviceRegistry, RegistryChanges, RegistrySnapshot,
};
pub use state::{AvailabilityChanges, Function, FunctionChanges, PhysicalDevice};

#[cfg(test)]
mod tests;
