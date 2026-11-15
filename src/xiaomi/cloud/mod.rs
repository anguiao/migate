mod client;
mod description;
mod error;
mod notifications;
mod oauth;
mod response;

pub use client::{CLOUD_BASE_URL, CloudClient, HomePage, TokenResponse};
pub use error::{CloudError, CloudErrorKind};
pub use notifications::{
    NotificationDriver, NotificationError, NotificationEvent, NotificationHandle,
    connect_notifications,
};

pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
pub(crate) mod tests;
