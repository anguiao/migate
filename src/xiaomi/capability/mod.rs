mod model;
mod outlet;

pub use model::{CapabilityError, ParsedOutlet, PropertyBinding};
pub(crate) use outlet::is_outlet_type;
pub use outlet::parse_outlet;

#[cfg(test)]
mod tests;
