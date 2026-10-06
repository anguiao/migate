mod catalog;
mod client;
mod error;
mod oauth;
mod properties;
mod response;

pub use catalog::{CloudDevice, OwnedHome, OwnedHomes, group_id};
pub use client::{CLOUD_BASE_URL, CloudClient, HomePage, TokenResponse};
pub use error::{CloudError, CloudErrorKind};
pub use oauth::{AUTHORIZATION_URL, AuthorizationAttempt, validate_saved_redirect_uri};

pub const REGION: &str = "cn";
pub const CLIENT_ID: u64 = 2_882_303_761_520_251_711;

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod tests;
