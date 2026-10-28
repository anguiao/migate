mod command;
mod curtain_command;
mod curtain_projection;
mod light_command;
mod light_observation;
mod model;
mod registry;
mod state;

pub use command::{
    ActionToken, CommandReceiver, CommandRequest, DeviceRequest, LightAction,
    LightActionDiagnostics, LightDimension, LightRequest,
};
pub use curtain_command::{CurtainAction, CurtainCommand, CurtainRequest, CurtainToken};
pub use light_command::{LightColorTarget, LightTarget};
pub(crate) use model::CurtainValue;
pub(crate) use model::mired_span;
pub use model::{
    Availability, BatterySnapshot, Capability, Command, CommandError, CurtainMotion,
    CurtainObservation, CurtainSnapshot, DeviceKey, FunctionDescription, FunctionKind,
    FunctionSnapshot, FunctionState, LightMode, LightPatch, LightProfile, LightProfileKind,
    LightSnapshot, MeasurementRange, MeasurementSnapshot, NumericRange, PhysicalDeviceDescription,
    Power, Rgb, StateInput, StateSnapshot, TemperatureInterval,
};
pub use registry::DeviceRegistry;
pub use state::{Function, PhysicalDevice};

#[cfg(test)]
mod tests;
