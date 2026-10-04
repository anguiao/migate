mod command;
mod model;
mod registry;
mod state;

pub use command::{CommandReceiver, CommandRequest};
pub use model::{
    Availability, Capability, Category, Command, CommandError, DeviceDescription, DeviceKey,
    DeviceSnapshot, FunctionDescription, FunctionKey, PhysicalDeviceDescription, Power,
};
pub use registry::{DeviceRegistry, RegistryChanges, RegistrySnapshot};
pub(crate) use state::Clock;
pub use state::{Changes, Device, Function, PhysicalDevice};

#[cfg(test)]
mod tests;
