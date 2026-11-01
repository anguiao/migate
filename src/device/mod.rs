pub(crate) mod air_conditioner;
mod air_conditioner_command;
mod command;
mod curtain_command;
mod curtain_projection;
mod fan_command;
mod light_command;
mod light_observation;
mod model;
mod registry;
mod state;
mod step_command;

pub(crate) use air_conditioner::AirConditionerValue;
pub use air_conditioner::{
    AirConditionerFanSpeed, AirConditionerMode, AirConditionerSnapshot, AirConditionerTarget,
    TemperatureGrid,
};
pub(crate) use air_conditioner_command::AirConditionerStep;
pub use air_conditioner_command::{
    AirConditionerAction, AirConditionerCommand, AirConditionerRequest,
};
pub use command::{
    ActionToken, CommandReceiver, CommandRequest, DeviceRequest, LightAction,
    LightActionDiagnostics, LightDimension, LightRequest,
};
pub use curtain_command::{CurtainAction, CurtainCommand, CurtainRequest};
pub(crate) use fan_command::FanStep;
pub use fan_command::{FanAction, FanCommand, FanRequest};
pub use light_command::{LightColorTarget, LightTarget};
pub(crate) use model::mired_span;
pub use model::{
    Availability, BatterySnapshot, Capability, Command, CommandError, CurtainMotion,
    CurtainSnapshot, DeviceKey, FanSnapshot, FunctionDescription, FunctionKind, FunctionSnapshot,
    FunctionState, LightMode, LightPatch, LightProfile, LightProfileKind, LightSnapshot,
    MeasurementRange, MeasurementSnapshot, NumericRange, PhysicalDeviceDescription, Power, Rgb,
    StateInput, StateSnapshot, TemperatureInterval, TemperatureSourceState,
};
pub(crate) use model::{CurtainValue, FanValue, FunctionKey};
pub use registry::DeviceRegistry;
pub use state::{Function, PhysicalDevice};

#[cfg(test)]
mod tests;
