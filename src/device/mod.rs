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
    Availability, Capability, Command, CommandError, DescriptionError, DeviceKey,
    FunctionDescription, FunctionKey, FunctionKind, FunctionSnapshot, LightMode, LightPatch,
    LightProfile, LightProfileKind, LightSnapshot, NumericRange, PhysicalDeviceDescription, Power,
    RangeError, Rgb, TemperatureInterval, brightness_from_matter_level, brightness_to_matter_level,
};
pub use registry::{DeviceRegistry, RegistryChanges, RegistrySnapshot};
pub use state::{AvailabilityChanges, Function, FunctionChanges, PhysicalDevice};

#[cfg(test)]
mod tests;
