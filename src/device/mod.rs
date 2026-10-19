mod command;
mod light_command;
mod model;
mod registry;
mod state;

pub use command::{
    ActionToken, CommandReceiver, CommandRequest, DeviceRequest, LightAction,
    LightActionDiagnostics, LightActionResult, LightDimension, LightRequest,
};
pub use light_command::{LightActionChanges, LightColorTarget, LightTarget};
pub(crate) use model::mired_span;
pub use model::{
    Availability, Capability, Command, CommandError, DescriptionError, DeviceKey,
    FunctionDescription, FunctionKey, FunctionKind, FunctionSnapshot, LightMode, LightPatch,
    LightProfile, LightProfileKind, LightSnapshot, NumericRange, PhysicalDeviceDescription, Power,
    RangeError, Rgb, TemperatureInterval,
};
pub use registry::{DeviceRegistry, RegistryChanges, RegistrySnapshot};
pub use state::{AvailabilityChanges, Function, FunctionChanges, PhysicalDevice};

#[cfg(test)]
mod tests;
