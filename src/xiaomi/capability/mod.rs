mod description;
mod model;
mod outlet;

pub use model::{CapabilityError, ParsedDevice, ParsedFunction, PropertyBinding};
pub(crate) use outlet::is_outlet_type;
pub use outlet::parse_device;

#[cfg(test)]
mod tests;
