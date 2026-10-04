mod description;
mod model;
mod outlet;

pub use model::{CapabilityError, ParsedDevice, ParsedFunction, ParsedOutlet, PropertyBinding};
pub(crate) use outlet::is_outlet_type;
pub use outlet::{parse_device, parse_outlet};

#[cfg(test)]
mod tests;
