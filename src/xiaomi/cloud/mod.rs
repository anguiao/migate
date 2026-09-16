mod client;
mod control;
mod error;
mod notification;
mod oauth;

pub use client::{
    CLOUD_BASE_URL, CloudClient, CloudDevice, HomePage, MIOT_SPEC_BASE_URL, OwnedCatalog,
    OwnedHome, OwnedRoom, TokenResponse,
};
pub use control::{
    CloudAction, CloudProperty, PropertyRead, PropertyReadOutcome, PropertyWrite,
    PropertyWriteOutcome, PropertyWriteResult,
};
pub use error::{CloudError, CloudErrorKind};
pub use notification::{
    CLOUD_MQTT_HOST, CLOUD_MQTT_PORT, CloudNotification, CloudNotificationError,
    CloudNotificationHandle, CloudNotificationSession,
};
pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod control_tests;
