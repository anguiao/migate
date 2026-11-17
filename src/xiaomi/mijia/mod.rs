mod action;
mod catalog;
mod client;
mod light_groups;
mod properties;
mod response;
mod rpc;
mod session;

pub(crate) use catalog::group_id;
pub(in crate::xiaomi) use catalog::{CloudDeviceInfo, OwnedHome};
pub(in crate::xiaomi) use catalog::{LanCandidate, LanCredentials};
pub(in crate::xiaomi) use client::MijiaClient;
pub(in crate::xiaomi) use light_groups::{
    CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
};
pub use session::MijiaSession;

#[cfg(test)]
mod tests;
