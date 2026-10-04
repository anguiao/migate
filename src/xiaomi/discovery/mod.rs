mod announcement;
mod browser;
mod model;

pub use announcement::SERVICE_TYPE;
pub use browser::{Discovery, DiscoveryChanges, DiscoveryDriver};
pub use model::{DiscoveryError, DiscoverySnapshot, Gateway, GatewayRoute};

#[cfg(test)]
mod tests;
