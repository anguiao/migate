mod command;
mod light_command;
mod model;
mod registry;
mod state;

pub use command::{CommandReceiver, CommandRequest};
pub use light_command::{
    ActionToken, DeviceRequest, LightAction, LightActionChanges, LightActionDiagnostics,
    LightActionResult, LightColorTarget, LightDimension, LightRequest, LightTarget,
};
pub use model::{
    Availability, Capability, Category, Command, CommandError, DeviceKey, FunctionDescription,
    FunctionKey, FunctionSnapshot, LightMode, LightPatch, LightSnapshot, NumericRange,
    PhysicalDeviceDescription, Power, Rgb, brightness_from_matter_level,
    brightness_to_matter_level,
};
pub use registry::{DeviceRegistry, RegistryChanges, RegistrySnapshot};
pub use state::{AvailabilityChanges, Function, FunctionChanges, PhysicalDevice};

#[cfg(test)]
mod tests;
