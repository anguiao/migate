mod action;
mod catalog;
mod client;
mod light_groups;
mod properties;
mod response;
mod rpc;
mod session;

pub use action::ActionResult;
pub use catalog::{CloudDevice, CloudDeviceInfo, OwnedHome, OwnedHomes, group_id};
pub(crate) use catalog::{LanCandidate, LanCredentials};
pub use client::{MIJIA_BASE_URL, MijiaClient};
pub use light_groups::{
    CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
};
pub use rpc::RpcOutcome;
pub use session::MijiaSession;

#[cfg(test)]
mod tests;
