mod client;
mod description;
mod error;
mod notifications;
mod oauth;
mod response;

pub use client::CloudClient;
pub(in crate::xiaomi) use client::TokenResponse;
pub use error::{CloudError, CloudErrorKind};
pub use notifications::NotificationError;
pub(in crate::xiaomi) use notifications::{
    NotificationDriver, NotificationEvent, NotificationHandle, connect_notifications,
};

pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
pub(crate) mod tests;
