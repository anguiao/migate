mod catalog;
mod client;
mod error;
mod light_groups;
mod notifications;
mod oauth;
mod properties;
mod response;

pub use catalog::{CloudDevice, CloudDeviceInfo, OwnedHome, OwnedHomes, group_id};
pub use client::{CLOUD_BASE_URL, CloudClient, HomePage, TokenResponse};
pub use error::{CloudError, CloudErrorKind};
pub use light_groups::{
    CloudCatalogDevice, CloudDeviceCatalog, CloudGroupMembership, CloudGroupStatus,
};
pub use notifications::{
    NotificationDriver, NotificationError, NotificationEvent, NotificationHandle,
    connect_notifications,
};

#[cfg(test)]
pub(crate) use notifications::tests::support as notification_test_support;
pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
mod tests;
